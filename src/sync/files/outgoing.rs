use ::std::sync::{Arc, RwLock, Mutex};
use ::sync::{SyncConfig, Syncer};
use ::sync::sync_model::SyncModel;
use ::sync::incoming::SyncIncoming;
use ::storage::Storage;
use ::api::{self, Api, ApiReq};
use ::messaging;
use ::error::{TResult, TError};
use ::models::file::FileData;
use ::models::sync_record::{SyncType, SyncRecord};
use ::std::fs;
use ::std::io::{Read, Write};

/// Holds the state for outgoing files (uploads)
pub struct FileSyncOutgoing {
    /// Holds our sync config. Note that this is shared between the sync system
    /// and the `Turtl` object in the main thread.
    config: Arc<RwLock<SyncConfig>>,

    /// Holds our Api object. Lets us chit chat with the Turtl server.
    api: Arc<Api>,

    /// Holds our user-specific db. This is mainly for persisting k/v data and
    /// for polling for file records that need uploading.
    db: Arc<Mutex<Option<Storage>>>,

    /// Stores our syn run version
    run_version: i64,
}

impl FileSyncOutgoing {
    /// Create a new outgoing syncer
    pub fn new(config: Arc<RwLock<SyncConfig>>, api: Arc<Api>, db: Arc<Mutex<Option<Storage>>>) -> Self {
        FileSyncOutgoing {
            config: config,
            api: api,
            db: db,
            run_version: 0,
        }
    }

    /// Looks at the first entry in the sync table for an outgoing file sync
    /// record. We could scan the whole table, but since syncs are in order and
    /// we really don't want to start uploading a file for a note that hasn't
    /// finished syncing, it only makes sense to check the front of the table
    /// for the sync record.
    fn get_next_outgoing_file_sync(&self) -> TResult<Option<SyncRecord>> {
        let next = with_db!{ db, self.db,
            SyncRecord::next(db)
        }?;
        match next {
            Some(x) => {
                match x.ty {
                    SyncType::FileOutgoing => {
                        if x.frozen {
                            Ok(None)
                        } else {
                            Ok(Some(x))
                        }
                    }
                    _ => Ok(None),
                }
            }
            None => { Ok(None) }
        }
    }

    /// Given a sync record for an outgoing file, find the corresponding file
    /// in our storage folder and stream it to our heroic API.
    fn upload_file(&mut self, sync: &mut SyncRecord) -> TResult<()> {
        let note_id = sync.item_id.clone();
        let user_id = {
            let local_config = self.get_config();
            let guard = lockr!(local_config);
            match guard.user_id.as_ref() {
                Some(x) => x.clone(),
                None => return TErr!(TError::MissingField(String::from("SyncConfig.user_id"))),
            }
        };

        #[derive(Deserialize, Debug)]
        struct UploadRes {
            #[serde(default)]
            #[serde(deserialize_with = "::util::ser::opt_vec_str_i64_converter::deserialize")]
            sync_ids: Option<Vec<i64>>,
        }

        // define a container function that grabs our file and runs the upload.
        // if anything in here fails, we mark 
        let upload = |note_id| -> TResult<UploadRes> {
            let file = FileData::file_finder(Some(&user_id), Some(note_id))?;
            info!("FileSyncOutgoing.upload_file() -- syncing file {:?}", file);
            // open our local file. we should test if it's readable/exists
            // before making API calls
            let mut file = fs::File::open(&file)?;
            // start our API call to the note file attachment endpoint
            let url = format!("/notes/{}/attachment", note_id);
            let req = ApiReq::new().header("Content-Type", &String::from("application/octet-stream")).timeout(60);
            // get an API stream we can start piping file data into
            let (mut stream, info) = self.api.call_start(api::Method::Put, &url[..], req)?;
            // start streaming our file into the API call 4K at a time
            let mut buf = [0; 4096];
            loop {
                let read = file.read(&mut buf[..])?;
                // all done! (EOF)
                if read <= 0 { break; }
                let (read_bytes, _) = buf.split_at(read);
                let written = stream.write(read_bytes)?;
                if read != written {
                    return TErr!(TError::Msg(format!("problem uploading file: grabbed {} bytes, only sent {} wtf wtf lol", read, written)));
                }
            }
            // write all our output and finalize the API call
            stream.flush()?;
            self.api.call_end(stream.send(), info)
        };

        match upload(&note_id) {
            Ok(res) => {
                match res.sync_ids.as_ref() {
                    Some(ids) => {
                        with_db!{ db, self.db,
                            // note that if we do have an error here, the worst that
                            // happens is we download the file right after uploading.
                            // so basically ignore errors.
                            match SyncIncoming::ignore_on_next(db, ids) {
                                Ok(_) => {},
                                Err(e) => error!("FileSyncOutgoing.upload() -- error ignoring sync items (but continuing regardless): {}", e),
                            }
                        };
                    }
                    None => {}
                }
            }
            Err(e) => {
                warn!("FileSyncOutgoing.run_sync() -- failed to upload file: {}", e);
                sync.set_error(&e);
                // our upload failed? send to our sync failure handler
                with_db!{ db, self.db,
                    SyncRecord::handle_failed_sync(db, sync)?;
                };
                // we've handled this, return ok, otherwise our main thread will
                // re-log the error which isn't but but kind of annoying
                return Ok(());
            }
        }

        // if we're still here, the upload succeeded. remove the sync record so
        // we know to stop trying to upload this file.
        with_db!{ db, self.db, sync.db_delete(db, None)? };

        // let the UI know how great we are. you will love this app. tremendous
        // app. everyone says so.
        messaging::ui_event("sync:file:uploaded", &json!({"note_id": note_id}))?;
        Ok(())
    }
}

impl Syncer for FileSyncOutgoing {
    fn get_name(&self) -> &'static str {
        "files:outgoing"
    }

    fn get_config(&self) -> Arc<RwLock<SyncConfig>> {
        self.config.clone()
    }

    fn get_delay(&self) -> u64 {
        1000
    }

    fn set_run_version(&mut self, run_version: i64) {
        self.run_version = run_version;
    }

    fn get_run_version(&self) -> i64 {
        self.run_version
    }

    fn run_sync(&mut self) -> TResult<()> {
        let sync_maybe = self.get_next_outgoing_file_sync()?;
        if let Some(mut sync) = sync_maybe {
            self.upload_file(&mut sync)?;
        }
        Ok(())
    }
}

