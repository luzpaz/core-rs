use ::turtl::Turtl;
use ::error::{TResult, TError};
use ::models::model::Model;
use ::models::protected::{Keyfinder, Protected};
use ::models::keychain::{Keychain, KeyRef};
use ::models::file::{File, FileData};
use ::models::sync_record::SyncAction;
use ::crypto::Key;
use ::sync::sync_model::{self, SyncModel, MemorySaver};
use ::std::fs;

protected! {
    #[derive(Serialize, Deserialize)]
    pub struct Note {
        #[protected_field(public)]
        pub space_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[protected_field(public)]
        pub board_id: Option<String>,
        #[serde(with = "::util::ser::int_converter")]
        #[protected_field(public)]
        pub user_id: String,
        #[serde(default)]
        #[protected_field(public)]
        pub has_file: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[protected_field(public, submodel)]
        pub file: Option<File>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "mod")]
        #[protected_field(public)]
        pub mod_: Option<i64>,

        #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
        #[protected_field(private)]
        pub type_: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[protected_field(private)]
        pub title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[protected_field(private)]
        pub tags: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[protected_field(private)]
        pub url: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[protected_field(private)]
        pub username: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[protected_field(private)]
        pub password: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[protected_field(private)]
        pub text: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[protected_field(private)]
        pub embed: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[protected_field(private)]
        pub color: Option<i64>,
    }
}

make_storable!(Note, "notes");
impl SyncModel for Note {}

impl Note {
    /// Remove the files attached to this note, if any.
    fn clear_files(&self) -> TResult<()> {
        // delete all local file(s) associated with this note
        let note_id = match self.id() {
            Some(x) => x.clone(),
            None => return TErr!(TError::MissingField(String::from("Note.id"))),
        };
        let files = FileData::file_finder_all(Some(&self.user_id), Some(&note_id))?;
        for file in files {
            fs::remove_file(&file)?;
        }
        Ok(())
    }

    /// Move a note to a different space
    pub fn move_spaces(&mut self, turtl: &Turtl, new_space_id: String) -> TResult<()> {
        self.space_id = new_space_id;
        sync_model::save_model(SyncAction::MoveSpace, turtl, self, false)?;
        Ok(())
    }
}

impl Keyfinder for Note {
    fn get_key_search(&self, turtl: &Turtl) -> TResult<Keychain> {
        let mut keychain = Keychain::new();
        let mut space_ids: Vec<String> = Vec::new();
        let mut board_ids: Vec<String> = Vec::new();
        space_ids.push(self.space_id.clone());
        if self.board_id.is_some() {
            board_ids.push(self.board_id.as_ref().unwrap().clone());
        }
        match self.get_keys() {
            Some(keys) => for key in keys {
                match key.get(&String::from("s")) {
                    Some(id) => space_ids.push(id.clone()),
                    None => {},
                }
                match key.get(&String::from("b")) {
                    Some(id) => board_ids.push(id.clone()),
                    None => {},
                }
            },
            None => {},
        }

        if space_ids.len() > 0 {
            let ty = String::from("space");
            let profile_guard = turtl.profile.read().unwrap();
            for space in &profile_guard.spaces {
                if space.id().is_none() || space.key().is_none() { continue; }
                let space_id = space.id().unwrap();
                if !space_ids.contains(space_id) { continue; }
                keychain.upsert_key(turtl, space_id, space.key().unwrap(), &ty)?;
            }
        }
        if board_ids.len() > 0 {
            let ty = String::from("board");
            let profile_guard = turtl.profile.read().unwrap();
            for board in &profile_guard.boards {
                if board.id().is_none() || board.key().is_none() { continue; }
                let board_id = board.id().unwrap();
                if !board_ids.contains(board_id) { continue; }
                keychain.upsert_key(turtl, board_id, board.key().unwrap(), &ty)?;
            }
        }
        Ok(keychain)
    }

    fn get_keyrefs(&self, turtl: &Turtl) -> TResult<Vec<KeyRef<Key>>> {
        let mut refs: Vec<KeyRef<Key>> = Vec::new();
        let profile_guard = turtl.profile.read().unwrap();
        for space in &profile_guard.spaces {
            if space.id() == Some(&self.space_id) && space.key().is_some() {
                refs.push(KeyRef {
                    id: self.space_id.clone(),
                    ty: String::from("s"),
                    k: space.key().unwrap().clone(),
                });
            }
        }

        match self.board_id {
            Some(ref board_id) => {
                for board in &profile_guard.boards {
                    if board.id() == Some(board_id) && board.key().is_some() {
                        refs.push(KeyRef {
                            id: board_id.clone(),
                            ty: String::from("b"),
                            k: board.key().unwrap().clone(),
                        });
                    }
                }
            },
            None => {},
        }
        Ok(refs)
    }
}

impl MemorySaver for Note {
    // reindex note on add/update (reindex is idempotent)
    fn mem_update(self, turtl: &Turtl, action: SyncAction) -> TResult<()> {
        match action {
            SyncAction::Add | SyncAction::Edit => {
                let note_id = match self.id() {
                    Some(x) => x.clone(),
                    None => return Ok(()),
                };
                let notes = turtl.load_notes(&vec![note_id])?;
                if notes.len() == 0 { return Ok(()); }
                let note = &notes[0];
                let mut search_guard = turtl.search.write().unwrap();
                match search_guard.as_mut() {
                    Some(ref mut search) => {
                        search.reindex_note(note)?;
                    }
                    // i COULD throw an error here. i'm choosing not to...
                    None => {}
                }
            }
            SyncAction::Delete => {
                let mut search_guard = turtl.search.write().unwrap();
                match search_guard.as_mut() {
                    Some(ref mut search) => search.unindex_note(&self)?,
                    // i COULD throw an error here. i'm choosing not to...
                    None => {},
                };

                self.clear_files()?;
            }
            _ => {}
        }
        Ok(())
    }
}

