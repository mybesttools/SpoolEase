use core::{any::Any, cell::RefCell};
use embassy_time::Instant;
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
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, channel::Channel};
use framework::{
    debug, error, info, mk_static,
    ntp::InstantExt,
    prelude::{Framework, SDCardStoreErrorSource},
    settings::{FILE_STORE_MAX_DIRS, FILE_STORE_MAX_FILES},
    term_error, term_info, warn,
};

use crate::{
    bambu::{FilamentInfo, KInfo, KNozzleId, TagInformation},
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

    #[snafu(display("Extended record format error"))]
    ExtFileUnread { error: String },

    #[snafu(display("Extended record format error"))]
    ExtFormat { source: serde_json::error::Error },
}

#[allow(clippy::enum_variant_names, dead_code)]
#[derive(Debug)]
pub enum WeightStoreDirective {
    ProvidedCurrentWeight(i32),
    UseStoreCurrentWeight,
}

#[derive(Debug)]
pub enum TagOperation {
    EncodeTag { weight: Option<i32>, set_encoded_as_new: Option<bool> },
    ReadTag,
    UpdateWeight { weight: i32 },
}

#[derive(Debug)]
pub enum StoreOp {
    WriteTag {
        tag_info: Box<TagInformation>,
        k_info: Option<KInfo>,
        tag_operation: TagOperation,
        // weight: WeightStoreDirective,
        cookie: Box<dyn AnyClone>,
    },
    ReadExtInfo {
        id: String,
        // if need several use cases, add cookie
    },
}

// Cookie - General code
pub trait AnyClone: Any + core::fmt::Debug {
    fn clone_box(&self) -> Box<dyn AnyClone>;
    fn into_any(self: Box<Self>) -> Box<dyn Any>;
    fn as_any(&self) -> &dyn Any;
}

pub trait Cookie: Any + Clone + core::fmt::Debug + 'static {}

impl<T> AnyClone for T
where
    T: Cookie, // Any + Clone  + core::fmt::Debug + 'static,
{
    fn clone_box(&self) -> Box<dyn AnyClone> {
        Box::new(self.clone())
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl Clone for Box<dyn AnyClone> {
    fn clone(&self) -> Box<dyn AnyClone> {
        self.clone_box()
    }
}

//

type StoreRequestsChannel = Channel<NoopRawMutex, StoreOp, 5>;
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
    pub requests_channel: &'static StoreRequestsChannel,
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
        let requests_channel = mk_static!(StoreRequestsChannel, StoreRequestsChannel::new());
        let store = Rc::new(Self {
            framework: framework.clone(),
            observers: RefCell::new(Vec::new()),
            requests_channel,
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

    pub fn notify_tag_stored(&self, result: Result<Option<(&SpoolRecord, &SpoolRecordExt)>, &str>, cookie: Box<dyn AnyClone>) {
        for weak_observer in self.observers.borrow().iter() {
            let observer = weak_observer.upgrade().unwrap();
            observer.borrow_mut().on_tag_stored(
                result.map(|s| s.map(|s| (s.0.clone(), s.1.clone()))).map_err(|e| e.to_string()),
                cookie.clone(),
            );
        }
    }

    pub fn notify_read_spool_record_ext(&self, result: Result<SpoolRecordExt, String>) {
        if let Some((last, rest)) = self.observers.borrow().split_last() {
            for weak_observer in rest.iter() {
                let observer = weak_observer.upgrade().unwrap();
                observer.borrow_mut().on_read_spool_record_ext(result.clone());
            }
            let observer = last.upgrade().unwrap();
            observer.borrow_mut().on_read_spool_record_ext(result);
        }
    }

    pub fn try_send_op(&self, op: StoreOp) -> Result<(), StoreError> {
        self.requests_channel.try_send(op).map_err(|_| StoreError::TooManyOps)
    }

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

    pub async fn add_untagged_spool(&self, mut spool_record: SpoolRecord) -> Result<String, StoreError> {
        let new_spool_id = (*self.last_spool_id.borrow()) + 1;
        if let Some(spools_db) = &self.spools_db.get() {
            spool_record.id = new_spool_id.to_string();
            spool_record.added_time = store_safe_time_now();
            match spools_db.insert(spool_record).await.context(CsvDbSnafu)? {
                true => {
                    *self.last_spool_id.borrow_mut() = new_spool_id;
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

    pub async fn edit_spool_from_web(&self, spool_record: SpoolRecord) -> Result<(), StoreError> {
        if let Some(spools_db) = &self.spools_db.get() {
            let updated_record = {
                let spools_db_borrow = spools_db.records.borrow(); // Important: Note this borrow, dropped when context ends, but if changing need to make sure it is dropped
                if let Some(current_record) = spools_db_borrow.get(&spool_record.id) {
                    // Taking this approach with extra clones, so if future fields are added, this won't be missed
                    let current_record = &current_record.data;
                    SpoolRecord {
                        id: spool_record.id,
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
                        ext_has_k: spool_record.ext_has_k,
                    }
                } else {
                    return Err(StoreError::NotFound { id: spool_record.id.clone() });
                }
            };

            spools_db.insert(updated_record).await.context(CsvDbSnafu)?;
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
            .map_err(|err| StoreError::ExtFileUnread {
                error: format!("{err} reading '{spool_rec_ext_file_path}'"),
            })?;
        let mut de = Deserializer::from_str(&ext_str);
        let spool_rec_ext = SpoolRecordExt::deserialize(&mut de).context(ExtFormatSnafu)?;
        // let spool_rec_ext = serde_json::from_str::<SpoolRecordExt>(&ext_str).context(ExtFormatSnafu)?;
        Ok(spool_rec_ext)
    }

    pub async fn update_spool(&self, spool_record: SpoolRecord) -> Result<(), StoreError> {
        if let Some(spools_db) = self.spools_db.get() {
            if !spool_record.id.is_empty() {
                if spools_db.records.borrow().contains_key(&spool_record.id) {
                    let tag_id = spool_record.tag_id.clone();
                    let id = spool_record.id.clone();
                    // TODO: ? theoretically need transaction mechanism here (so lock db and then do the index operation as well)
                    spools_db.insert(spool_record).await.context(CsvDbSnafu)?;
                    if !tag_id.is_empty() {
                        self.tag_id_index.borrow_mut().insert(tag_id, id);
                    } else {
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
                    Ok(())
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
                            match TagInformation::from_descriptor(tag_desciptor) {
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

    let receiver = store.requests_channel.receiver();
    loop {
        match receiver.receive().await {
            StoreOp::WriteTag {
                tag_info,
                k_info,
                tag_operation,
                cookie,
            } => {
                let mut spool_rec; // to database and to file

                if let Some(spools_db) = store.spools_db.get() {
                    let request_tag_id = tag_id_hex(tag_info.tag_id.as_ref().unwrap());
                    let request_spool_id = tag_info.id.clone();
                    // debug!(">>>> on entry use_spool_id={use_spool_id}");

                    let use_spool_id; // "" if need to add, otherwise the one to use
                    let matching_spool_id = store.tag_id_index.borrow().get(&request_tag_id).cloned();
                    let matching_tag_id = if let Some(request_spool_id) = request_spool_id.as_ref() {
                        if let Some((tag_id, _spool_id)) = store.tag_id_index.borrow().iter().find(|v| v.1 == request_spool_id) {
                            Some(tag_id.clone())
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    // This is tricky - here is the logic, below in code comments are repeated for clarity
                    // The outtpub is eventually use_spool_id - which record to write to eventually, existing and which or new
                    //
                    // If there isn't an ID
                    //   if found matching_spool_id for tag_id
                    //     if encode-tag
                    //       strike matching_spool_id
                    //       create new record
                    //     if read-tag
                    //       use matching spool_id
                    //   else
                    //     create new record
                    //
                    // If there is ID
                    //   if it isn't tagged
                    //     if found matching_spool_id for request_tag_id
                    //       strike matching_spool_id
                    //     use ID
                    //   else (the ID is tagged already)
                    //     if matching_tag_id == request_tag_id
                    //       Use ID
                    //     else
                    //       if found matching_spool_id for request_tag_id
                    //         strike old record with request_tag_id
                    //       use new record (because it is used already, no switches of tags on same record)

                    match tag_info.id {
                        None => {
                            // If there isn't an ID
                            if let Some(matching_spool_id) = matching_spool_id {
                                //   if found matching_spool_id for tag_id
                                match tag_operation {
                                    TagOperation::EncodeTag {
                                        weight: _,
                                        set_encoded_as_new: _,
                                    } => {
                                        //     if encode-tag
                                        //       strike matching_spool_id
                                        // TODO: to function
                                        let mut record_to_strike = {
                                            let records_borrow = spools_db.records.borrow();
                                            let record_wrapper = records_borrow.get(&matching_spool_id).unwrap();
                                            record_wrapper.data.clone()
                                        };
                                        record_to_strike.tag_id = format!("-{}", record_to_strike.tag_id);
                                        let _ = spools_db.insert(record_to_strike).await;
                                        store.tag_id_index.borrow_mut().remove(&request_tag_id);

                                        //       create new record
                                        use_spool_id = None;
                                    }
                                    TagOperation::ReadTag => {
                                        //       use matching spool_id
                                        use_spool_id = Some(matching_spool_id.clone());
                                    }
                                    TagOperation::UpdateWeight { weight: _ } => {
                                        error!("Error: Update weight without ID");
                                        store.notify_tag_stored(Err("Internal Software Error, update weight Spool-ID not found"), cookie);
                                        continue;
                                    }
                                }
                            } else {
                                //     create new record
                                use_spool_id = None;
                            }
                        }
                        Some(request_spool_id) => {
                            // If there is ID
                            match matching_tag_id {
                                None => {
                                    //   if it isn't tagged
                                    if let Some(matching_spool_id) = matching_spool_id {
                                        //     if found matching_spool_id for request_tag_id
                                        //       strike matching_spool_id
                                        let mut record_to_strike = {
                                            let records_borrow = spools_db.records.borrow();
                                            let record_wrapper = records_borrow.get(&matching_spool_id).unwrap();
                                            record_wrapper.data.clone()
                                        };
                                        record_to_strike.tag_id = format!("-{}", record_to_strike.tag_id);
                                        let _ = spools_db.insert(record_to_strike).await;
                                        store.tag_id_index.borrow_mut().remove(&request_tag_id);
                                    }
                                    //     use ID
                                    use_spool_id = Some(request_spool_id.clone());
                                }
                                Some(matching_tag_id) => {
                                    //   else (the ID is tagged already)
                                    if request_tag_id == matching_tag_id {
                                        //     if matching_tag_id == request_tag_id
                                        //       Use ID
                                        use_spool_id = Some(request_spool_id.clone());
                                    } else {
                                        //     if matching_tag_id == request_tag_id
                                        if matching_tag_id == request_tag_id {
                                            use_spool_id = Some(request_spool_id.clone());
                                        } else {
                                            //       if found matching_spool_id for request_tag_id
                                            if let Some(matching_spool_id) = matching_spool_id {
                                                //         strike old record with request_tag_id
                                                let mut record_to_strike = {
                                                    let records_borrow = spools_db.records.borrow();
                                                    let record_wrapper = records_borrow.get(&matching_spool_id).unwrap();
                                                    record_wrapper.data.clone()
                                                };
                                                record_to_strike.tag_id = format!("-{}", record_to_strike.tag_id);
                                                let _ = spools_db.insert(record_to_strike).await;
                                                store.tag_id_index.borrow_mut().remove(&request_tag_id);
                                            }
                                            //       use new record (because it is used already, no switches of tags on same record)
                                            use_spool_id = None;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    info!("use_spool_id = {use_spool_id:?}");

                    // if let Some(spools_db) = store.spools_db.get() {
                    //     if !use_spool_id.is_empty() && !spools_db.records.borrow().contains_key(&use_spool_id) {
                    //         error!("Software Logic Error, encoded from id that isn't found");
                    //         store.notify_tag_stored(Err(&format!("Internal Software Error, encoded ID not found {use_spool_id}")), cookie);
                    //         continue;
                    //     }
                    //     let spool_id_from_tag_id_clone = store.tag_id_index.borrow().get(&tag_id_hex).cloned();
                    //     if let Some(spool_id_from_tag_id) = spool_id_from_tag_id_clone {
                    //         // debug!(">>>>> Found tag_id {spool_id_from_tag_id}");
                    //         if !use_spool_id.is_empty() && spool_id_from_tag_id != use_spool_id {
                    //             // debug!(">>>> spool_id_from_tag_id ({spool_id_from_tag_id} != use_spool_id {use_spool_id}");
                    //             // This is a case where encoding using an inventory spool-id when the tag_id is already in use
                    //             // in such case we need to 'strikeout' the tag_id (add a "-" to the beginning)
                    //             // practically this means tag_id doen't exist after we are done here
                    //             let mut record_to_strike = {
                    //                 let records_borrow = spools_db.records.borrow();
                    //                 let record_wrapper = records_borrow.get(&spool_id_from_tag_id).unwrap();
                    //                 record_wrapper.data.clone()
                    //             };
                    //             record_to_strike.tag_id = format!("-{}", record_to_strike.tag_id);
                    //             let _ = spools_db.insert(record_to_strike).await;
                    //             store.tag_id_index.borrow_mut().remove(&tag_id_hex);
                    //             tag_id_already_exist = false;
                    //         } else if let Some(current_rec) = spools_db.records.borrow().get(&spool_id_from_tag_id) {
                    //             // debug!(
                    //             //     ">>>> spool_id_from_tag_id ({spool_id_from_tag_id} == use_spool_id {use_spool_id}, and gained access to the record"
                    //             // );
                    //             // get the record, should exist if got here, if not fatal error
                    //             existing_record_current_weight = current_rec.data.weight_current;
                    //             existing_record_current_note = current_rec.data.note.clone();
                    //             use_spool_id = current_rec.data.id.clone();
                    //             tag_id_already_exist = true;
                    //         } else {
                    //             error!("Fatal Error: Internal error in tag_id to spool_id mapping, tag exist but not found");
                    //             store.notify_tag_stored(Err("Internal software error, tag exists but not found"), cookie);
                    //             continue;
                    //         }
                    //     } else {
                    //         // debug!(">>>>> tag_id doesn't exist (1)");
                    //         tag_id_already_exist = false;
                    //     }
                    // } else {
                    //     // debug!(">>>>> tag_id doesn't exist (2)");
                    //     tag_id_already_exist = false;
                    //     mistake here, this else is on no access to db
                    // }

                    let (id, added_new_record) = match use_spool_id {
                        Some(existing_spool_id) => (existing_spool_id, false),
                        None => ((*store.last_spool_id.borrow() + 1).to_string(), true),
                    };

                    let curr_record = if added_new_record {
                        None
                    } else {
                        #[allow(clippy::collapsible_if)]
                        if let Some(curr_csv_rec) = spools_db.records.borrow().get(&id) {
                            Some(curr_csv_rec.data.clone())
                        } else {
                            store.notify_tag_stored(Err(&format!("Internal Software Error\n Using existing record not found {id}")), cookie);
                            continue;
                        }
                    };

                    let tag_info_filament_info = tag_info.filament.unwrap_or(FilamentInfo::new());
                    spool_rec = SpoolRecord {
                        id: id.clone(),
                        tag_id: request_tag_id.clone(),
                        slicer_filament: tag_info_filament_info.tray_info_idx,
                        material_type: tag_info_filament_info.tray_type,
                        material_subtype: tag_info.filament_subtype.unwrap_or_default(),
                        color_name: tag_info.color_name.unwrap_or_default(),
                        color_code: tag_info_filament_info.tray_color,
                        note: tag_info.note.unwrap_or_default(), // this is the only (?) field where data in the encode can be changed in store, so there's special handling later
                        brand: tag_info.brand.unwrap_or_default(),
                        weight_advertised: tag_info.weight_advertised,
                        weight_core: tag_info.weight_core,
                        weight_new: tag_info.weight_new,
                        encode_time: tag_info.encode_time,
                        weight_current: curr_record.as_ref().and_then(|rec| rec.weight_current),
                        added_time: curr_record.as_ref().and_then(|rec| rec.added_time).or(store_safe_time_now()), // a field that isn't coming from tag
                        added_full: curr_record.as_ref().and_then(|rec| rec.added_full), // a field that isn't coming from tag
                        consumed_since_add: curr_record.as_ref().map_or_else(|| 0.0, |rec| rec.consumed_since_add), // a field that isn't coming from tag
                        consumed_since_weight: curr_record.as_ref().map_or_else(|| 0.0, |rec| rec.consumed_since_weight), // a field that isn't coming from tag
                        ext_has_k: if k_info.is_some() {
                            true
                        } else {
                            curr_record.as_ref().map_or_else(|| false, |rec| rec.ext_has_k)
                        },
                    };

                    match tag_operation {
                        TagOperation::EncodeTag { weight, set_encoded_as_new } => {
                            if let Some(weight) = weight {
                                spool_rec.weight_current = Some(weight);
                                spool_rec.consumed_since_weight = 0.0; // updating current weight should clear the consumed since_weight
                            }
                            if set_encoded_as_new.is_some() {
                                spool_rec.added_full = set_encoded_as_new;
                            }
                            spool_rec.encode_time = store_safe_time_now();
                        }
                        TagOperation::ReadTag => {
                            // if we read a tag, with a note, the record note takes precedence and should override what's in the tag
                            if let Some(existing_spool_rec) = curr_record {
                                spool_rec.note = existing_spool_rec.note;
                            }
                        }
                        TagOperation::UpdateWeight { weight } => {
                            // if we update weight, with a note coming from tag, the record note takes precedence and should override what's in the tag
                            if let Some(existing_spool_rec) = curr_record {
                                spool_rec.note = existing_spool_rec.note;
                            }
                            spool_rec.weight_current = Some(weight);
                            spool_rec.consumed_since_weight = 0.0; // updating ccurrent weight should clear the consumed since_weight
                        }
                    }

                    if !added_new_record {
                        if let Some(current_rec) = spools_db.records.borrow().get(&id) {
                            if current_rec.data.added_time.is_some() {
                                spool_rec.added_time = current_rec.data.added_time;
                            }
                        }
                    }

                    // debug!(">>>> Storing {spool_rec:?}");

                    match spools_db.insert(spool_rec.clone()).await {
                        Ok(true) => {
                            info!("Stored tag to spools database");
                        }
                        Ok(false) => {
                            info!("Stored tag to spools database, but no change");
                        }
                        Err(e) => {
                            error!("Error storing record to spools database {e}");
                            store.notify_tag_stored(Err(&format!("Failed to store Tag\n{e}")), cookie);
                            continue;
                        }
                    }

                    // Store of record succeeded and case of new record added, so need to update index and last_spool_id
                    if added_new_record {
                        *store.last_spool_id.borrow_mut() = id.parse().unwrap();
                    }

                    store.tag_id_index.borrow_mut().insert(request_tag_id, id.clone());

                    //////////////////////////////////////////////////////////////////////////////////////////
                    // Write extr info file  ////////////////////////////////////////////////////////////////
                    //////////////////////////////////////////////////////////////////////////////////////////

                    let spool_rec_ext = SpoolRecordExt {
                        tag: Some(tag_info.origin_descriptor),
                        k_info,
                    };

                    match store.store_spool_rec_ext(&spool_rec.id, &spool_rec_ext).await {
                        Ok(file_path) => {
                            info!("Stored extra spool {} information to file '{file_path}'", spool_rec.id);
                            store.notify_tag_stored(Ok(Some((&spool_rec, &spool_rec_ext))), cookie.clone());
                        }
                        Err(err) => {
                            error!("Error writing tag {} : {err:?}", spool_rec.id);
                            store.notify_tag_stored(Err("Inventory updated,\nbut failed writing extended info,\ncheck logs"), cookie);
                        }
                    }

                    // // TODO: switch to save_spool_rec_ext() (Done, above)
                    // if let Ok(spool_rec_ext_file_path) = spool_rec_ext_file_path(&spool_rec.id) {
                    //     let file_store = framework.borrow().file_store();
                    //     let mut file_store = file_store.lock().await;
                    //     match serde_json::to_string(&spool_rec_ext) {
                    //         Ok(s) => match file_store.create_write_file_str(&spool_rec_ext_file_path, &s).await {
                    //             Ok(_) => {
                    //                 info!("Stored extra spool {} information to file {spool_rec_ext_file_path}", spool_rec.id);
                    //             }
                    //             Err(err) => {
                    //                 error!("Error writing tag file to {spool_rec_ext_file_path} : {err}");
                    //                 store.notify_tag_stored(Err("Inventory updated,\nbut failed writing extended info (1),\ncheck logs"), cookie);
                    //                 continue;
                    //             }
                    //         },
                    //         Err(e) => {
                    //             error!("Error serializing tag information to store: {e}");
                    //             store.notify_tag_stored(Err("Inventory updated,\nbut failed writing extended info (2),\ncheck logs"), cookie);
                    //             continue;
                    //         }
                    //     }
                    //     store.notify_tag_stored(Ok(Some((&spool_rec, &spool_rec_ext))), cookie.clone());
                    // } else {
                    //     error!("Internal Error: Trying to store ext with bad id : {id}");
                    //     store.notify_tag_stored(Err(&format!("Internal Error: Trying to store ext with bad id : {id}")), cookie);
                    //     continue;
                    // }
                } else {
                    store.notify_tag_stored(Ok(None), cookie.clone());
                    continue;
                }
            }
            StoreOp::ReadExtInfo { id } => {
                let res = if let Ok(spool_rec_ext_file_path) = spool_rec_ext_file_path(&id) {
                    let file_store = framework.borrow().file_store();
                    let mut file_store = file_store.lock().await;
                    match file_store.read_file_str(&spool_rec_ext_file_path).await {
                        Ok(ext_str) => match serde_json::from_str::<SpoolRecordExt>(&ext_str) {
                            Ok(ext) => Ok(ext),
                            Err(err) => {
                                error!("Error parsing spool extra information : {err:?}");
                                Err("Error parsing spool extra information".to_string())
                            }
                        },
                        Err(err) => {
                            error!("Error loading tag information : {err:?}");
                            Err("Error loading spool extra information : {err}".to_string())
                        }
                    }
                } else {
                    error!("Internal Error: Requested spool extra info with bad Id : {id}");
                    Err("Internal Error: Requested spool extra info with bad Id : {id}".to_string())
                };
                store.notify_read_spool_record_ext(res);
            }
        }
    }
}

// TODO: think if to change it to get the spoolRecord from store (and it will hold Rc to store)
#[derive(Debug, Clone)]
pub struct FullSpoolRecord {
    pub spool_rec: SpoolRecord,
    pub spool_rec_ext: SpoolRecordExt,
}

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
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
    // pub encoded: Option<()>,
    // #[serde(default,deserialize_with = "deserialize_optional_unit")]
    // pub dried: Option<()>,
    // #[serde(default,deserialize_with = "deserialize_optional_unit")]
    // pub used: Option<()>,
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
    fn on_tag_stored(&mut self, result: Result<Option<(SpoolRecord, SpoolRecordExt)>, String>, cookie: Box<dyn AnyClone>); // String result is id of stored tag
    fn on_read_spool_record_ext(&mut self, result: Result<SpoolRecordExt, String>);
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
