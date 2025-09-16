use core::{any::Any, cell::RefCell};
use embassy_time::{Instant, Timer};
use hashbrown::HashMap;
use once_cell::unsync::OnceCell;
use serde::{Deserialize, Serialize};
use serde_json::Deserializer;
use shared::utils::{
    deserialize_bool_yn_empty_n, deserialize_f32_base64, deserialize_optional, deserialize_optional_bool_yn, serialize_bool_yn, serialize_f32_base64,
    serialize_optional_bool_yn,
};
use snafu::prelude::*;

use alloc::{
    boxed::Box,
    format,
    rc::Rc,
    string::{String, ToString},
    vec::Vec,
};
// use embassy_sync::{blocking_mutex::raw::NoopRawMutex, channel::Channel};
use framework::{
    debug, error, info, mk_static,
    ntp::InstantExt,
    prelude::{Framework, SDCardStoreErrorSource},
    settings::{FILE_STORE_MAX_DIRS, FILE_STORE_MAX_FILES},
    term_error, term_info, warn,
};

use crate::{
    bambu::{KInfo, KNozzleId, TagInformation},
    csvdb::{CsvDb, CsvDbError, CsvDbId},
    view_model::ViewModel,
};

// #[derive(Snafu, Debug)]
// pub enum InternalError {
//     BadId,
// }
const STORE_VER: &str = "1.1.0";

#[derive(Snafu, Debug)]
pub enum StoreError {
    #[snafu(display("Too many store operations pending"))]
    TooManyOps,

    #[snafu(display("CsvDbError : {source:?}"))]
    CsvDbError { source: CsvDbError },

    #[snafu(display("SDCard File Operation Error {source:?}"))]
    Store { source: SDCardStoreErrorSource },

    #[snafu(display("Internal store software logic error"))]
    InternalError,

    #[snafu(display("Record not found"))]
    NotFound { id: String },

    #[snafu(display("Can't access databse (SD Card Installed?)"))]
    NoCsvDb,

    #[snafu(display("Missing required id for operation in record"))]
    MissingId,

    #[snafu(display("Bad Id for operation"))]
    BadId,

    #[snafu(display("Id not found in databse"))]
    IdNotFound,

    #[snafu(display("Can't read extended file"))]
    ExtFileReadFailure { error: String },

    #[snafu(display("Extended record format error"))]
    ExtFormat { source: serde_json::error::Error },
}

// #[allow(clippy::enum_variant_names, dead_code)]
// #[derive(Debug)]
// pub enum WeightStoreDirective {
//     ProvidedCurrentWeight(i32),
//     UseStoreCurrentWeight,
// }
//
// #[derive(Debug)]
// pub enum StoreOp {
//     ReadExtInfo {
//         id: String,
//         // if need several use cases, add cookie
//     },
// }

// DON'T ERASE - May be useful in the future
// // Cookie - General code
// pub trait AnyClone: Any + core::fmt::Debug {
//     fn clone_box(&self) -> Box<dyn AnyClone>;
//     fn into_any(self: Box<Self>) -> Box<dyn Any>;
//     fn as_any(&self) -> &dyn Any;
// }
//
// pub trait Cookie: Any + Clone + core::fmt::Debug + 'static {}
//
// impl<T> AnyClone for T
// where
//     T: Cookie, // Any + Clone  + core::fmt::Debug + 'static,
// {
//     fn clone_box(&self) -> Box<dyn AnyClone> {
//         Box::new(self.clone())
//     }
//
//     fn into_any(self: Box<Self>) -> Box<dyn Any> {
//         self
//     }
//
//     fn as_any(&self) -> &dyn Any {
//         self
//     }
// }
//
// impl Clone for Box<dyn AnyClone> {
//     fn clone(&self) -> Box<dyn AnyClone> {
//         self.clone_box()
//     }
// }

//

// type StoreRequestsChannel = Channel<NoopRawMutex, StoreOp, 5>;
// type StoreRequestsReceiver<'a> = Receiver::<'a, NoopRawMutex, StoreOp, 5>;

// embedded_hal_bus::spi::ExclusiveDevice<esp_hal::spi::master::Spi<'_, esp_hal::Async>, esp_hal::gpio::Output<'_>, embedded_hal_bus::spi::NoDelay>
type TheSpi = embedded_hal_bus::spi::ExclusiveDevice<
    esp_hal::spi::master::Spi<'static, esp_hal::Async>,
    esp_hal::gpio::Output<'static>,
    embedded_hal_bus::spi::NoDelay,
>;

#[allow(private_interfaces)]
pub struct Store {
    framework: Rc<RefCell<Framework>>,
    observers: RefCell<Vec<alloc::rc::Weak<RefCell<dyn StoreObserver>>>>,
    // pub requests_channel: &'static StoreRequestsChannel,
    // TODO: make spools_db mutext or something that doesn't need borrow
    // Think if need to make the entire store under mutex (if there are several related dbs could case issues)
    pub spools_db: OnceCell<CsvDb<SpoolRecord, TheSpi, 20, 5>>,
    last_spool_id: RefCell<i32>,
    tag_id_index: RefCell<HashMap<String, String>>,
    pub initialized: RefCell<bool>,
    store_rc: RefCell<Option<Rc<Store>>>,
}

impl Store {
    pub fn new(framework: Rc<RefCell<Framework>>) -> Rc<Store> {
        // let requests_channel = mk_static!(StoreRequestsChannel, StoreRequestsChannel::new());
        let store = Rc::new(Self {
            framework: framework.clone(),
            observers: RefCell::new(Vec::new()),
            // requests_channel,
            spools_db: OnceCell::new(),
            last_spool_id: RefCell::new(0),
            tag_id_index: RefCell::new(HashMap::new()),
            initialized: RefCell::new(false),
            store_rc: RefCell::new(None),
        });
        *store.store_rc.borrow_mut() = Some(store.clone());
        store
    }

    pub fn start(&self, view_model: Rc<RefCell<ViewModel>>) {
        let store = self.store_rc.borrow_mut().clone().unwrap();
        self.framework
            .borrow()
            .spawner
            .spawn(store_task(self.framework.clone(), store, view_model))
            .ok();
    }

    pub fn subscribe(&self, observer: alloc::rc::Weak<RefCell<dyn StoreObserver>>) {
        self.observers.borrow_mut().push(observer);
    }

    // pub fn notify_read_spool_record_ext(&self, result: Result<SpoolRecordExt, String>) {
    //     if let Some((last, rest)) = self.observers.borrow().split_last() {
    //         for weak_observer in rest.iter() {
    //             let observer = weak_observer.upgrade().unwrap();
    //             observer.borrow_mut().on_read_spool_record_ext(result.clone());
    //         }
    //         let observer = last.upgrade().unwrap();
    //         observer.borrow_mut().on_read_spool_record_ext(result);
    //     }
    // }

    // pub fn try_send_op(&self, op: StoreOp) -> Result<(), StoreError> {
    //     self.requests_channel.try_send(op).map_err(|_| StoreError::TooManyOps)
    // }

    pub fn is_available(&self) -> bool {
        self.spools_db.get().is_some()
    }
    pub fn is_initialized(&self) -> bool {
        *self.initialized.borrow()
    }

    pub fn query_spools(&self) -> Option<String> {
        if let Some(spools_db) = self.spools_db.get() {
            let spool_records = spools_db.records.borrow();
            let total_length = spool_records.values().map(|v| v.length).sum::<usize>();
            let results: Result<String, CsvDbError> = spool_records.values().try_fold(String::with_capacity(total_length), |mut acc, v| {
                let csv = v.to_csv_string();
                if let Err(e) = &csv {
                    error!("Error serializing to csv: {v:?} : {e}");
                }
                acc.push_str(&csv?);
                Ok(acc)
            });
            // TODO: make it an error up as well, to handle in the caller
            match results {
                Ok(s) => Some(s),
                Err(_) => None,
            }
        } else {
            None
        }
    }

    pub async fn delete_spool(&self, id: &str) -> Result<(), StoreError> {
        let deleted_record = if let Some(spools_db) = &self.spools_db.get() {
            let delete_res = spools_db.delete(id).await;
            if let Ok(Some(record)) = &delete_res {
                self.tag_id_index.borrow_mut().remove(&record.tag_id);
            }
            delete_res.context(CsvDbSnafu)?
        } else {
            None
        };

        if let Some(deleted_record) = deleted_record {
            if !deleted_record.tag_id.is_empty() {
                if let Ok(spool_rec_ext_file_path) = spool_rec_ext_file_path(&deleted_record.id) {
                    let file_store = self.framework.borrow().file_store();
                    let mut file_store = file_store.lock().await;
                    let _ = file_store.delete_file(&spool_rec_ext_file_path).await;
                }
            }
        }
        Ok(())
    }

    pub async fn add_spool(&self, mut spool_rec: SpoolRecord, spool_rec_ext: SpoolRecordExt) -> Result<String, StoreError> {
        let new_spool_id = (*self.last_spool_id.borrow()) + 1;
        if let Some(spools_db) = &self.spools_db.get() {
            spool_rec.id = new_spool_id.to_string();
            spool_rec.added_time = store_safe_time_now();
            spool_rec.ext_has_k = spool_rec_ext.k_info.is_some();
            let tag_id = spool_rec.tag_id.clone();
            let id = spool_rec.id.clone();
            match spools_db.insert(spool_rec).await.context(CsvDbSnafu)? {
                true => {
                    *self.last_spool_id.borrow_mut() = new_spool_id;
                    // deal with tag_id
                    let tag_id_update_res = if !tag_id.is_empty() {
                        // check if tag_id was with some other record, if so remove that mapping and 'strikeout' that spool_record
                        let update_res = if let Some(mut existing_spool_rec_with_tag_id) = self.get_spool_by_hex_tag(&tag_id) {
                            existing_spool_rec_with_tag_id.tag_id = format!("-{}", existing_spool_rec_with_tag_id.tag_id);
                            self.update_spool(existing_spool_rec_with_tag_id, None).await.map(|_| ())
                        } else {
                            Ok(())
                        };
                        self.tag_id_index.borrow_mut().insert(tag_id, id);
                        update_res
                    } else {
                        Ok(())
                    };
                    self.store_spool_rec_ext(&new_spool_id.to_string(), &spool_rec_ext).await?;
                    tag_id_update_res?; // want to perform all operations and report error if anything happened in the middle
                    Ok(new_spool_id.to_string())
                }
                false => {
                    error!("Internal error, add spool added an already existing spool");
                    Err(StoreError::InternalError)
                }
            }
        } else {
            error!("Internal error, can't access store");
            Err(StoreError::NoCsvDb)
        }
    }

    pub async fn edit_spool_from_web(&self, spool_record: SpoolRecord, k_info: Option<KInfo>) -> Result<(), StoreError> {
        if let Some(spools_db) = &self.spools_db.get() {
            let updated_record = {
                let spools_db_borrow = spools_db.records.borrow(); // Important: Note this borrow, dropped when context ends, but if changing need to make sure it is dropped
                if let Some(current_record) = spools_db_borrow.get(&spool_record.id) {
                    // Taking this approach with extra clones, so if future fields are added, this won't be missed
                    let current_record = &current_record.data;
                    SpoolRecord {
                        id: spool_record.id.clone(),
                        tag_id: current_record.tag_id.clone(), // can't change from web
                        material_type: spool_record.material_type,
                        material_subtype: spool_record.material_subtype,
                        color_name: spool_record.color_name,
                        color_code: spool_record.color_code,
                        note: spool_record.note,
                        brand: spool_record.brand,
                        weight_advertised: spool_record.weight_advertised,
                        weight_core: spool_record.weight_core,
                        weight_new: current_record.weight_new,         // can't change from web
                        weight_current: current_record.weight_current, // can't change from web
                        slicer_filament: spool_record.slicer_filament,
                        added_time: current_record.added_time.or(store_safe_time_now()), // in case somehow no added date (ntp) then add it now
                        encode_time: current_record.encode_time,
                        added_full: spool_record.added_full,
                        consumed_since_add: current_record.consumed_since_add,
                        consumed_since_weight: current_record.consumed_since_weight,
                        ext_has_k: k_info.is_some(),
                    }
                } else {
                    return Err(StoreError::NotFound { id: spool_record.id.clone() });
                }
            };

            spools_db.insert(updated_record).await.context(CsvDbSnafu)?;
            let mut spool_rec_ext = match self.get_spool_ext_by_id(&spool_record.id).await {
                Ok(spool_rec_ext) => spool_rec_ext,
                Err(err) => {
                    error!("Error loading extended info when editing file, using empty as baseline for edit: {err:?}");
                    SpoolRecordExt::default()
                }
            };
            spool_rec_ext.k_info = k_info;
            self.store_spool_rec_ext(&spool_record.id, &spool_rec_ext).await?;
            Ok(())
        } else {
            Err(StoreError::NoCsvDb)
        }
    }
    pub fn get_spool_by_hex_tag(&self, tag_id_hex: &str) -> Option<SpoolRecord> {
        if let Some(spools_db) = self.spools_db.get() {
            if let Some(spool_id) = self.tag_id_index.borrow().get(tag_id_hex) {
                if let Some(current_rec) = spools_db.records.borrow().get(spool_id) {
                    return Some(current_rec.data.clone());
                }
            }
        }
        None
    }
    pub fn get_spool_by_tag_id(&self, tag_id: &[u8]) -> Option<SpoolRecord> {
        self.get_spool_by_hex_tag(&tag_id_hex(tag_id))
    }

    pub fn get_spool_by_id(&self, id: &str) -> Option<SpoolRecord> {
        if let Some(spools_db) = self.spools_db.get() {
            if let Some(current_rec) = spools_db.records.borrow().get(id) {
                return Some(current_rec.data.clone());
            }
        }
        None
    }

    // TODO: once working, use it in other places reading ext
    pub async fn get_spool_ext_by_id(&self, id: &str) -> Result<SpoolRecordExt, StoreError> {
        if self.get_spool_by_id(id).is_none() {
            return Err(StoreError::NotFound { id: id.to_string() });
        }
        let spool_rec_ext_file_path = spool_rec_ext_file_path(id).map_err(|_| StoreError::NotFound { id: id.to_string() })?;
        let file_store = self.framework.borrow().file_store();
        let mut file_store = file_store.lock().await;
        let ext_str = file_store
            .read_file_str(&spool_rec_ext_file_path)
            .await
            .map_err(|err| StoreError::ExtFileReadFailure {
                error: format!("{err} reading '{spool_rec_ext_file_path}'"),
            })?;
        let mut de = Deserializer::from_str(&ext_str);
        let spool_rec_ext = SpoolRecordExt::deserialize(&mut de).context(ExtFormatSnafu)?;
        // let spool_rec_ext = serde_json::from_str::<SpoolRecordExt>(&ext_str).context(ExtFormatSnafu)?;
        Ok(spool_rec_ext)
    }

    #[allow(clippy::type_complexity)]
    pub async fn update_spool(
        &self,
        mut spool_record: SpoolRecord,
        update_ext_fn: Option<Box<dyn FnOnce(&mut SpoolRecordExt)>>,
    ) -> Result<Option<SpoolRecordExt>, StoreError> {
        let mut ret_spool_rec_ext = None;
        if let Some(spools_db) = self.spools_db.get() {
            if !spool_record.id.is_empty() {
                if spools_db.records.borrow().contains_key(&spool_record.id) {
                    if let Some(update_ext_fn) = update_ext_fn {
                        let mut spool_rec_ext = self.get_spool_ext_by_id(&spool_record.id).await.ok().unwrap_or_default(); // on read error don't raise error
                        update_ext_fn(&mut spool_rec_ext);
                        spool_record.ext_has_k = spool_rec_ext.k_info.is_some();
                        self.store_spool_rec_ext(&spool_record.id, &spool_rec_ext).await?;
                        ret_spool_rec_ext = Some(spool_rec_ext);
                    }
                    let tag_id = spool_record.tag_id.clone();
                    let id = spool_record.id.clone();
                    // TODO: ? theoretically need transaction mechanism here (so lock db and then do the index operation as well)
                    spools_db.insert(spool_record).await.context(CsvDbSnafu)?;
                    if !tag_id.is_empty() && !tag_id.starts_with("-") {
                        // first search if this tag_id is in use already and strike it out before adding this tag to index
                        if let Some(mut existing_spool_rec_with_tag_id) = self.get_spool_by_hex_tag(&tag_id) {
                            if existing_spool_rec_with_tag_id.id != id {
                                existing_spool_rec_with_tag_id.tag_id = format!("-{}", existing_spool_rec_with_tag_id.tag_id);
                                spools_db.insert(existing_spool_rec_with_tag_id).await.context(CsvDbSnafu)?;
                            }
                        }
                        self.tag_id_index.borrow_mut().insert(tag_id, id);
                    } else {
                        // for some reason tag_id was removed
                        let tag_id = self
                            .tag_id_index
                            .borrow()
                            .iter()
                            .find(|(_, index_id)| *index_id == &id)
                            .map(|(index_tag, _)| index_tag.clone());
                        if let Some(tag_id) = tag_id {
                            self.tag_id_index.borrow_mut().remove(&tag_id);
                        }
                    }
                    Ok(ret_spool_rec_ext)
                } else {
                    error!("Internal error, can't access store");
                    Err(StoreError::NoCsvDb)
                }
            } else {
                Err(StoreError::IdNotFound)
            }
        } else {
            Err(StoreError::MissingId)
        }
    }

    pub async fn store_spool_rec_ext(&self, id: &str, spool_rec_ext: &SpoolRecordExt) -> Result<String, StoreError> {
        let spool_rec_ext_file_path = spool_rec_ext_file_path(id)?;
        let file_store = self.framework.borrow().file_store();
        let mut file_store = file_store.lock().await;
        let s = serde_json::to_string(&spool_rec_ext).map_err(|_err| StoreError::InternalError)?;
        file_store.create_write_file_str(&spool_rec_ext_file_path, &s).await.context(StoreSnafu)?;
        Ok(spool_rec_ext_file_path)
    }

    #[allow(unused_variables)]
    pub async fn upgrade_versions(
        &self,
        db_version: semver::Version,
        current_version: semver::Version,
        view_model: Rc<RefCell<ViewModel>>,
    ) -> Result<(), StoreError> {
        if let Some(spools_db) = self.spools_db.get() {
            let spool_ids: Vec<_> = {
                let records = spools_db.records.borrow();
                records.keys().cloned().collect()
            };
            for spool_id in spool_ids {
                info!("Upgrading store spool {spool_id}");
                let mut spool_rec_ext = SpoolRecordExt::default();
                match self.get_spool_ext_by_id(&spool_id).await {
                    Ok(loaded_spool_rec_ext) => {
                        spool_rec_ext = loaded_spool_rec_ext;
                        if let Some(tag_desciptor) = &spool_rec_ext.tag {
                            match TagInformation::from_v1_descriptor(tag_desciptor) {
                                Ok(tag_info) => {
                                    if !tag_info.calibrations.is_empty() {
                                        let k_info = view_model.borrow().get_k_info_from_old_tag(&tag_info);
                                        if let Some(k_info) = k_info {
                                            info!("Upgrading spool {}, adding k_info {:?} to extended info", spool_id, k_info);
                                            spool_rec_ext.k_info = Some(k_info);
                                        }
                                    }
                                }
                                Err(err) => {
                                    error!("Error parsing tag descriptor for spool {}, ignoring : {err:?}", spool_id);
                                    // Store anyway, since there were issues with old files that needs to be fixed
                                }
                            }
                        } else {
                            warn!("No tag descriptor found for spool {}, ignoring", spool_id);
                        }
                    }
                    Err(err) => {
                        error!("Error reading extra data for spool {}, ignoring : {err:?}", spool_id);
                    }
                }
                // Store anyway, since there were issues with old files that needs to be fixed (writing small file on larger file leave extra in file)
                // and potentially past versions with missing files
                if let Err(err) = self.store_spool_rec_ext(&spool_id, &spool_rec_ext).await {
                    // TODO: undo upgrade and restore old version of file system?
                    error!("Error storing ext data for spool {}, ignoring : {err:?}", spool_id);
                } else {
                    spools_db.records.borrow_mut().get_mut(&spool_id).unwrap().data.ext_has_k = spool_rec_ext.k_info.is_some();
                }
            }
            spools_db.save_all_records_only_before_use().await.context(CsvDbSnafu)?;
            spools_db.update_version(STORE_VER).await.context(CsvDbSnafu)?;
        }
        Ok(())
    }
}

#[embassy_executor::task] // up to two printers in parallel
pub async fn store_task(framework: Rc<RefCell<Framework>>, store: Rc<Store>, view_model: Rc<RefCell<ViewModel>>) {
    let db_available;
    {
        debug!("Started store_task");
        let file_store = framework.borrow().file_store();
        match CsvDb::<SpoolRecord, _, FILE_STORE_MAX_DIRS, FILE_STORE_MAX_FILES>::new(file_store.clone(), "/store/spools", 1024, 200, STORE_VER).await
        {
            Ok(mut db) => match db.start(true, true).await {
                Ok(_) => {
                    let mut db_version = {
                        let db_inner = db.inner.borrow();
                        db_inner.db_meta.version.clone()
                    };
                    if db_version == "1" {
                        db_version = "1.0.0".to_string();
                    }
                    match semver::Version::parse(db_version.as_str()) {
                        Ok(db_version) => {
                            let current_version = semver::Version::parse(STORE_VER).unwrap();
                            if current_version < db_version {
                                term_info!(
                                    "Critical Error: Store version is {}, this firmware supports up to {}",
                                    db_version,
                                    current_version
                                );
                                db_available = false;
                            } else {
                                // currently upgrade is only for ext, so done after loading the db
                                store
                                    .spools_db
                                    .set(db)
                                    .map_err(|_e| "Fatal Internal Error: Can't assign spools_db to once_cell?")
                                    .unwrap();
                                term_info!("Opened spools database");

                                if current_version > db_version {
                                    info!("!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!");
                                    term_info!("Upgrading store from {} to {}", db_version, current_version);
                                    info!("!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!");
                                    if let Err(err) = store.upgrade_versions(db_version, current_version, view_model.clone()).await {
                                        info!("!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!");
                                        term_error!("Error upgrading store : {:?}", err);
                                        info!("!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!");
                                        db_available = false;
                                    } else {
                                        info!("!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!");
                                        term_info!("Store upgrade completed successfully");
                                        info!("!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!");
                                        db_available = true;
                                    }
                                } else {
                                    db_available = true;
                                }
                            }
                        }
                        Err(err) => {
                            term_error!("Unparsable store DB version {} {:?}", db_version, err);
                            db_available = false;
                        }
                    }
                }
                Err(e) => {
                    term_error!("Failed to start spools database (and load data): {:?}", e);
                    db_available = false;
                }
            },
            Err(e) => {
                term_error!("Failed to open spools database : {}", e);
                db_available = false;
            }
        }
    }

    // find largest_id in list, would be better if we persisted that

    let mut largest_id = 0;
    if db_available {
        if let Some(spools_db) = store.spools_db.get() {
            let records = spools_db.records.borrow();
            for record in records.iter() {
                if let Ok(id) = record.1.data.id.parse::<i32>() {
                    if !record.1.data.tag_id.is_empty() && record.1.data.tag_id.as_bytes()[0] != b'-' {
                        store
                            .tag_id_index
                            .borrow_mut()
                            .insert(record.1.data.tag_id.clone(), record.1.data.id.clone());
                    }
                    if id > largest_id {
                        largest_id = id;
                    }
                }
            }
        }
    }
    *store.last_spool_id.borrow_mut() = largest_id;
    *store.initialized.borrow_mut() = true;

    // let receiver = store.requests_channel.receiver();
    loop {
        Timer::after_secs(60).await;
        // match receiver.receive().await {
        // }
    }
}

// TODO: think if to change it to get the spoolRecord from store (and it will hold Rc to store)
#[derive(Debug, Clone, Default)]
pub struct FullSpoolRecord {
    pub spool_rec: SpoolRecord,
    pub spool_rec_ext: SpoolRecordExt,
}

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone, Default)]
pub struct SpoolRecord {
    pub id: String,
    pub tag_id: String,           // 14 (7*2)
    pub material_type: String,    // 10
    pub material_subtype: String, // 10
    pub color_name: String,       // 10
    pub color_code: String,       // 8
    pub note: String,             // 40
    pub brand: String,            // 30
    #[serde(deserialize_with = "deserialize_optional")]
    pub weight_advertised: Option<i32>, // 4
    #[serde(deserialize_with = "deserialize_optional")]
    pub weight_core: Option<i32>, // 4
    #[serde(deserialize_with = "deserialize_optional")]
    pub weight_new: Option<i32>, // 4
    #[serde(deserialize_with = "deserialize_optional")]
    pub weight_current: Option<i32>, // 4
    #[serde(default)]
    pub slicer_filament: String,
    #[serde(default, deserialize_with = "deserialize_optional")]
    pub added_time: Option<i32>,
    #[serde(default, deserialize_with = "deserialize_optional")]
    pub encode_time: Option<i32>,
    #[serde(default, serialize_with = "serialize_optional_bool_yn", deserialize_with = "deserialize_optional_bool_yn")]
    pub added_full: Option<bool>,
    #[serde(default, serialize_with = "serialize_f32_base64", deserialize_with = "deserialize_f32_base64")]
    pub consumed_since_add: f32,
    #[serde(default, serialize_with = "serialize_f32_base64", deserialize_with = "deserialize_f32_base64")]
    pub consumed_since_weight: f32,
    #[serde(default, serialize_with = "serialize_bool_yn", deserialize_with = "deserialize_bool_yn_empty_n")]
    pub ext_has_k: bool,
    // #[serde(default,deserialize_with = "deserialize_optional_unit")]
    // pub price: Option<()>,
    // #[serde(default,deserialize_with = "deserialize_optional_unit")]
    // pub quality: Option<()>,
    // #[serde(default,deserialize_with = "deserialize_optional_unit")]
    // pub diameter: Option<()>,
    //
    // #[serde(default,deserialize_with = "deserialize_optional_unit")]
    // pub location: Option<()>,
    //
    // #[serde(default,deserialize_with = "deserialize_optional_unit")]
    // pub purchased: Option<()>,
    // #[serde(default,deserialize_with = "deserialize_optional_unit")]
    // pub opened: Option<()>,
    // #[serde(default,deserialize_with = "deserialize_optional_unit")]
    // pub dried: Option<()>,
    // #[serde(default,deserialize_with = "deserialize_optional_unit")]
    // pub last_used: Option<()>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct SpoolRecordExt {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub k_info: Option<KInfo>,
}

impl SpoolRecordExt {
    pub fn get_calibrations(&self, printer: &str, extruder: i32, diameter: &str, nozzle_id: &str) -> Option<&KNozzleId> {
        let res = self
            .k_info
            .as_ref()?
            .printers
            .get(printer)?
            .extruders
            .get(&extruder)?
            .diameters
            .get(diameter)?
            .nozzles
            .get(nozzle_id);
        res
    }
}

impl CsvDbId for SpoolRecord {
    fn id(&self) -> &String {
        &self.id
    }
}

pub trait StoreObserver {
    // fn on_read_spool_record_ext(&mut self, result: Result<SpoolRecordExt, String>);
}

fn tag_id_hex(tag_id: &[u8]) -> String {
    hex::encode_upper(tag_id)
}

fn spool_rec_ext_file_path(ext_rec_id: &str) -> Result<String, StoreError> {
    if let Ok(id_num) = ext_rec_id.parse::<i32>() {
        let folder_num = ((id_num / 16) % 16) + 1;
        let file_path = format!("/store/spools.ext/{folder_num}/{id_num}.jsn");
        Ok(file_path)
    } else {
        Err(StoreError::BadId)
    }
}

pub fn store_safe_time_now() -> Option<i32> {
    Instant::now().to_date_time().map(|date_time| date_time.timestamp() as i32)
}
