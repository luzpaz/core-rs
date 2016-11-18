//! The Turtl module is the container for the state of the app. It provides
//! functions/interfaces for updating or retrieving stateful info, and is passed
//! around to various pieces of the app running in the main thread.

use ::std::sync::{Arc, RwLock};
use ::std::ops::Drop;
use ::std::collections::HashMap;

use ::regex::Regex;
use ::futures::{self, Future};
use ::num_cpus;

use ::jedi::{self, Value};
use ::config;

use ::error::{TResult, TFutureResult, TError};
use ::util::event::{self, Emitter};
use ::storage::{self, Storage};
use ::api::Api;
use ::profile::Profile;
use ::models::protected::{self, Keyfinder, Protected};
use ::models::model::Model;
use ::models::user::User;
use ::models::keychain::{self, KeyRef};
use ::util::thredder::{Thredder, Pipeline};
use ::messaging::{Messenger, Response};
use ::sync::{self, SyncConfig, SyncState};

/// Defines a container for our app's state. Note that most operations the user
/// has access to via messaging get this object passed to them.
pub struct Turtl {
    /// Our phone channel to the main thread. Although not generally used
    /// directly by the Turtl object, Turtl may spawn other processes that need
    /// it (eg after login the sync system needs it) to it's handy to have a
    /// copy laying around.
    pub tx_main: Pipeline,
    /// This is our app-wide event bus.
    pub events: event::EventEmitter,
    /// Holds our current user (Turtl only allows one logged-in user at once)
    pub user: RwLock<User>,
    /// Holds the user's data profile (keychain, boards, notes, etc, etc, etc)
    pub profile: RwLock<Profile>,
    /// Need to do some CPU-intensive work and have a Future finished when it's
    /// done? Send it here! Great for decrypting models.
    pub work: Thredder,
    /// Need to do some I/O and have a Future finished when it's done? Send it
    /// here! Great for API calls.
    pub async: Thredder,
    /// Allows us to send messages to our UI
    pub msg: Messenger,
    /// A storage system dedicated to key-value data. This *must* be initialized
    /// before our main local db because our local db is baed off the currently
    /// logged-in user, and we need persistant key-value storage even when
    /// logged out.
    pub kv: Arc<Storage>,
    /// Our main database, initialized after a successful login. This db is
    /// named via a function of the user ID and the server we're talking to,
    /// meaning we can have multiple databases that store different things for
    /// different people depending on server/user.
    pub db: RwLock<Option<Arc<Storage>>>,
    /// Our external API object. Note that most things API-related go through
    /// the Sync system, but there are a handful of operations that Sync doesn't
    /// handle that need API access (Personas (soon to be deprecated) and
    /// invites come to mind). Use sparingly.
    pub api: Arc<Api>,
    /// Sync system configuration (shared state with the sync system).
    pub sync_config: Arc<RwLock<SyncConfig>>,
    /// Holds our sync state data
    sync_state: Arc<RwLock<Option<SyncState>>>,
}

/// A handy type alias for passing Turtl around
pub type TurtlWrap = Arc<Turtl>;

impl Turtl {
    /// Create a new Turtl app
    fn new(tx_main: Pipeline) -> TResult<Turtl> {
        let num_workers = num_cpus::get() - 1;

        let api = Arc::new(Api::new());
        let data_folder = config::get::<String>(&["data_folder"])?;
        let kv_location = if data_folder == ":memory:" {
            String::from(":memory:")
        } else {
            format!("{}/kv.sqlite", &data_folder)
        };
        let kv = Arc::new(Storage::new(&kv_location, jedi::obj())?);

        // make sure we have a client id
        storage::setup_client_id(kv.clone())?;

        let turtl = Turtl {
            tx_main: tx_main.clone(),
            events: event::EventEmitter::new(),
            user: RwLock::new(User::new()),
            profile: RwLock::new(Profile::new()),
            api: api,
            msg: Messenger::new(),
            work: Thredder::new("work", tx_main.clone(), num_workers as u32),
            async: Thredder::new("async", tx_main.clone(), 2),
            kv: kv,
            db: RwLock::new(None),
            sync_config: Arc::new(RwLock::new(SyncConfig::new())),
            sync_state: Arc::new(RwLock::new(None)),
        };
        Ok(turtl)
    }

    /// A handy wrapper for creating a wrapped Turtl object (TurtlWrap),
    /// shareable across threads.
    pub fn new_wrap(tx_main: Pipeline) -> TResult<TurtlWrap> {
        let turtl = Arc::new(Turtl::new(tx_main)?);
        Ok(turtl)
    }

    /// Send a message to (presumably) our UI.
    pub fn remote_send(&self, id: Option<String>, msg: String) -> TResult<()> {
        match id {
            Some(id) => self.msg.send_suffix(id, msg),
            None => self.msg.send(msg),
        }
    }

    /// Send a success response to a remote request
    pub fn msg_success(&self, mid: &String, data: Value) -> TResult<()> {
        let res = Response {
            e: 0,
            d: data,
        };
        let msg = jedi::stringify(&res)?;
        self.remote_send(Some(mid.clone()), msg)
    }

    /// Send an error response to a remote request
    pub fn msg_error(&self, mid: &String, err: &TError) -> TResult<()> {
        let res = Response {
            e: 1,
            d: Value::String(format!("{}", err)),
        };
        let msg = jedi::stringify(&res)?;
        self.remote_send(Some(mid.clone()), msg)
    }

    /// Log a user in
    pub fn login(&self, username: String, password: String) -> TFutureResult<()> {
        self.with_next_fut()
            .and_then(move |turtl| -> TFutureResult<()> {
                let turtl2 = turtl.clone();
                User::login(turtl.clone(), &username, &password)
                    .and_then(move |_| -> TFutureResult<()> {
                        let db = try_fut!(turtl2.create_user_db());
                        let mut db_guard = turtl2.db.write().unwrap();
                        *db_guard = Some(Arc::new(db));
                        drop(db_guard);
                        futures::finished(()).boxed()
                    })
                    .boxed()
            })
            .boxed()
    }

    /// Log a user out
    pub fn logout(&self) -> TFutureResult<()> {
        self.with_next_fut()
            .and_then(|turtl| -> TFutureResult<()> {
                turtl.events.trigger("sync:shutdown", &Value::Bool(false));
                try_fut!(User::logout(turtl.clone()));

                // wipe the user db
                let mut db_guard = turtl.db.write().unwrap();
                *db_guard = None;
                futures::finished(()).boxed()
            })
            .boxed()
    }

    /// Given that our API is synchronous but we need to not block the main
    /// thread, we wrap it here such that we can do all the setup/teardown of
    /// handing the Api object off to a closure that runs inside of our `async`
    /// runner.
    pub fn with_api<F>(&self, cb: F) -> TFutureResult<Value>
        where F: FnOnce(Arc<Api>) -> TResult<Value> + Send + Sync + 'static
    {
        let api = self.api.clone();
        self.async.run(move || {
            cb(api)
        })
    }

    /// Start our sync system. This should happen after a user is logged in, and
    /// we definitely have a Turtl.db object available.
    pub fn start_sync(&self) -> TResult<()> {
        // create the ol' in/out (in/out) db connections for our sync system
        let db_out = Arc::new(self.create_user_db()?);
        let db_in = Arc::new(self.create_user_db()?);
        // start the sync, and save the resulting state into Turtl
        let sync_state = sync::start(self.tx_main.clone(), self.sync_config.clone(), self.api.clone(), db_out, db_in)?;
        {
            let mut state_guard = self.sync_state.write().unwrap();
            *state_guard = Some(sync_state);
        }

        // set up some bindings to manage the sync system easier
        self.with_next(|turtl| {
            let turtl1 = turtl.clone();
            turtl.events.bind_once("app:shutdown", move |_| {
                turtl1.with_next(|turtl| {
                    turtl.events.trigger("sync:shutdown", &jedi::obj());
                });
            }, "turtl:app:shutdown:sync");

            let sync_state1 = turtl.sync_state.clone();
            let sync_state2 = turtl.sync_state.clone();
            let sync_state3 = turtl.sync_state.clone();
            turtl.events.bind_once("sync:shutdown", move |joinval| {
                let join = match *joinval {
                    Value::Bool(x) => x,
                    _ => false,
                };
                let mut guard = sync_state1.write().unwrap();
                if guard.is_some() {
                    let state = guard.as_mut().unwrap();
                    (state.shutdown)();
                    if join {
                        loop {
                            let hn = state.join_handles.pop();
                            match hn {
                                Some(x) => match x.join() {
                                    Ok(_) => (),
                                    Err(e) => error!("turtl -- sync:shutdown: problem joining thread: {:?}", e),
                                },
                                None => break,
                            }
                        }
                    }
                }
                *guard = None;
            }, "turtl:sync:shutdown");
            turtl.events.bind("sync:pause", move |_| {
                let guard = sync_state2.read().unwrap();
                if guard.is_some() { (guard.as_ref().unwrap().pause)(); }
            }, "turtl:sync:pause");
            turtl.events.bind("sync:resume", move |_| {
                let guard = sync_state3.read().unwrap();
                if guard.is_some() { (guard.as_ref().unwrap().resume)(); }
            }, "turtl:sync:resume");
        });
        Ok(())
    }

    /// Run the given callback on the next main loop. Essentially gives us a
    /// setTimeout (if you are familiar). This means we can do something after
    /// the stack is unwound, but get a fresh Turtl context for our callback.
    ///
    /// Very useful for (un)binding events and such while inside of another
    /// triggered event (which normally deadlocks).
    ///
    /// Also note that this doesn't call the `cb` with `Turtl`, but instead
    /// `TurtlWrap` which is also nice because we can `.clone()` it and use it
    /// in multiple callbacks.
    pub fn with_next<F>(&self, cb: F)
        where F: FnOnce(TurtlWrap) + Send + Sync + 'static
    {
        self.tx_main.next(cb);
    }

    /// Return a future that resolves with a TurtlWrap object on the next main
    /// loop.
    pub fn with_next_fut(&self) -> TFutureResult<TurtlWrap> {
        self.tx_main.next_fut()
    }

    /// Create a new per-user database for the current user.
    pub fn create_user_db(&self) -> TResult<Storage> {
        let db_location = self.get_user_db_location()?;
        let dumpy_schema = config::get::<Value>(&["schema"])?;
        Storage::new(&db_location, dumpy_schema)
    }

    /// Get the physical location of the per-user database file we will use for
    /// the current logged-in user.
    pub fn get_user_db_location(&self) -> TResult<String> {
        let user_guard = self.user.read().unwrap();
        let user_id = match user_guard.id() {
            Some(x) => x,
            None => return Err(TError::MissingData(String::from("turtl.get_user_db_location() -- user.id() is None (can't open db without an ID)"))),
        };
        let data_folder = config::get::<String>(&["data_folder"])?;
        if data_folder == ":memory:" {
            return Ok(String::from(":memory:"));
        }
        let api_endpoint = config::get::<String>(&["api", "endpoint"])?;
        let re = Regex::new(r"(?i)[^a-z0-9]")?;
        let server = re.replace_all(&api_endpoint, "");
        Ok(format!("{}/turtl-user-{}-srv-{}.sqlite", data_folder, user_id, server))
    }

    /// Given a model that we suspect we have a key entry for, find that model's
    /// key, set it into the model, and return a reference to the key.
    pub fn find_model_key<'a, T>(&self, model: &'a mut T) -> TResult<Option<&'a Vec<u8>>>
        where T: Protected + Keyfinder
    {
        fn found_key<'a, T>(model: &'a mut T, key: Vec<u8>) -> TResult<Option<&'a Vec<u8>>>
            where T: Protected
        {
            model.set_key(Some(key));
            return Ok(model.key());
        }

        let profile_guard = self.profile.read().unwrap();
        let ref keychain = profile_guard.keychain;

        // check the keychain right off the bat. it's quick and easy, and most
        // entries are going to be here anyway
        if model.id().is_some() {
            match keychain.find_entry(model.id().unwrap()) {
                Some(key) => return found_key(model, key),
                None => {},
            }
        }

        let mut search = model.get_key_search(self);
        let encrypted_keys: Vec<HashMap<String, String>> = match model.get_keys() {
            Some(x) => x.clone(),
            None => Vec::new(),
        };

        // if we have no self-decrypting keys, and there's no keychain entry for
        // this model, then there's no way we can find a key
        if encrypted_keys.len() == 0 { return Ok(None); }

        let encrypted_keys: Vec<KeyRef<String>> = encrypted_keys.into_iter()
            .map(|entry| keychain::keyref_from_encrypted(&entry))
            .filter(|x| x.k != "")
            .collect::<Vec<_>>();

        // push the user's key into our search, if it's available
        {
            let user_guard = self.user.read().unwrap();
            if user_guard.id().is_some() && user_guard.key().is_some() {
                search.add_key(user_guard.id().unwrap(), user_guard.id().unwrap(), user_guard.key().unwrap(), &String::from("user"));
            }
        }

        // no direct keychain entry
        for keyref in &encrypted_keys {
            let ref encrypted_key = keyref.k;
            let ref object_id = keyref.id;

            // check if this object is in the keychain first. if so, we can use
            // its key to decrypt our encrypted key
            match keychain.find_entry(object_id) {
                Some(decrypting_key) => {
                    match protected::decrypt_key(&decrypting_key, encrypted_key) {
                        Ok(key) => return found_key(model, key),
                        Err(_) => {},
                    }
                },
                None => {},
            }

            // check our search object for matches
            let matches = search.find_all_entries(object_id);
            for key in &matches {
                match protected::decrypt_key(key, encrypted_key) {
                    Ok(key) => return found_key(model, key),
                    Err(_) => {},
                }
            }
        }
        Ok(None)
    }

    /// Shut down this Turtl instance and all the state/threads it manages
    pub fn shutdown(&mut self) { }
}

// Probably don't need this since `shutdown` just wipes our internal state which
// would happen anyway it Turtl is dropped, but whatever.
impl Drop for Turtl {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ::config;
    use ::jedi;

    use ::crypto;
    use ::util::thredder::Pipeline;
    use ::models::model::Model;
    use ::models::protected::Protected;
    use ::models::user;
    use ::models::note::Note;
    use ::models::board::Board;

    protected!{
        pub struct Dog {
            ( user_id: String ),
            ( name: String ),
            ( )
        }
    }

    /// Give us a new Turtl to start running tests on
    fn with_test(logged_in: bool) -> Turtl {
        config::set(&["data_folder"], &String::from(":memory:")).unwrap();
        let turtl = Turtl::new(Pipeline::new()).unwrap();
        if logged_in {
            let mut user_guard = turtl.user.write().unwrap();
            let version = 0;    // version 0 is much quicker...
            let (key, auth) = user::generate_auth(&String::from("timmy@killtheradio.net"), &String::from("gfffft"), version).unwrap();
            user_guard.id = Some(String::from("0158745252dbaf227c2eb2aca9cd869887e3f394033a7cd25f467f67dcf68a1a6699c3023ba033e1"));
            user_guard.do_login(key, auth);
        }
        turtl
    }

    #[test]
    fn finding_keys() {
        let note_key = crypto::from_base64(&String::from("eVWebXDGbqzDCaYeiRVsZEHsdT5WXVDnL/DdmlbqN2c=")).unwrap();
        let board_key = crypto::from_base64(&String::from("BkRzt6lu4YoTS9opB96c072y+kt+evtXv90+ZXHfsG8=")).unwrap();
        let enc_board = String::from(r#"{"body":"AAUCAAHeI0ysDNAenXpPAlOwQmmHzNWcohaCSmRXOPiRGVojaylzimiohTBSG2DyPnfsSXBl+LfxXhA=","keys":[],"user_id":"5244679b2b1375384f0000bc","id":"01549210bd2db6e84d965f99d2741739cf417b7df52f51008c55035365bc734b25fb2acbf5c9007c"}"#);
        let enc_note = String::from(r#"{"boards":["01549210bd2db6e84d965f99d2741739cf417b7df52f51008c55035365bc734b25fb2acbf5c9007c"],"mod":1479425965,"keys":[{"b":"01549210bd2db6e84d965f99d2741739cf417b7df52f51008c55035365bc734b25fb2acbf5c9007c","k":"AAUCAAECDLI141jXNUwVadmvUuxXYtWZ+JL7450VjH1JURk0UigiIB2TQ2f5KiDGqZKUoHyxFXCaAeorkaXKxCaAqicISg=="}],"user_id":"5244679b2b1375384f0000bc","body":"AAUCAAGTaDVBJHRXgdsfHjrI4706aoh6HKbvoa6Oda4KP0HV07o4JEDED/QHqCVMTCODJq5o2I3DNv0jIhZ6U3686ViT6YIwi3EUFjnE+VMfPNdnNEMh7uZp84rUaKe03GBntBRNyiGikxn0mxG86CGnwBA8KPL1Gzwkxd+PJZhPiRz0enWbOBKik7kAztahJq7EFgCLdk7vKkhiTdOg4ghc/jD6s9ATeN8NKA90MNltzTIM","id":"015874a823e4af227c2eb2aca9cd869887e3f394033a7cd25f467f67dcf68a1a6699c3023ba0361f"}"#);
        let mut board: Board = jedi::parse(&enc_board).unwrap();
        let mut note: Note = jedi::parse(&enc_note).unwrap();

        let turtl = with_test(true);
        let user_id = {
            let user_guard = turtl.user.read().unwrap();
            user_guard.id().unwrap().clone()
        };

        // add the note's key as a direct entry to the keychain
        let mut profile_guard = turtl.profile.write().unwrap();
        profile_guard.keychain.add_key(&user_id, note.id().unwrap(), &note_key, &String::from("note"));
        drop(profile_guard);

        // see if we can find the note as a direct entry
        {
            let found_key = turtl.find_model_key(&mut note).unwrap().unwrap();
            assert_eq!(found_key, &note_key);
        }

        // clear out the keychain, and add the board's key to the keychain
        let mut profile_guard = turtl.profile.write().unwrap();
        profile_guard.keychain.entries.clear();
        assert_eq!(profile_guard.keychain.entries.len(), 0);
        profile_guard.keychain.add_key(&user_id, board.id().unwrap(), &board_key, &String::from("board"));
        assert_eq!(profile_guard.keychain.entries.len(), 1);
        drop(profile_guard);

        // we should be able to find the board's key, if we found the note's key
        // but it's good to be sure
        {
            let found_key = turtl.find_model_key(&mut board).unwrap().unwrap();
            assert_eq!(found_key, &board_key);
        }

        // ok, now the real test...can we find the note's key by one layer of
        // indirection? (in other words, the note has no keychain entry, so it
        // searches the keychain for it's note.keys.b record, and uses that key
        // (if found) to decrypt its own key
        {
            let found_key = turtl.find_model_key(&mut note).unwrap().unwrap();
            assert_eq!(found_key, &note_key);
        }

        // clear out the keychain. we're going to see if the note's
        // get_key_search() function works for us
        let mut profile_guard = turtl.profile.write().unwrap();
        profile_guard.keychain.entries.clear();
        // put the board into the profile
        profile_guard.boards.push(board);
        assert_eq!(profile_guard.keychain.entries.len(), 0);
        drop(profile_guard);

        // empty keychain...this basically forces the find_model_key() fn to
        // use the model's get_key_search() function, which is custom for the
        // note type to search based on board keys
        {
            let found_key = turtl.find_model_key(&mut note).unwrap().unwrap();
            assert_eq!(found_key, &note_key);
        }

    }
}

