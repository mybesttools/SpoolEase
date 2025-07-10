use crate::csvdb::deserialize_optional;
use core::{any::Any, cell::RefCell};
use embassy_time::Instant;
use hashbrown::HashMap;
use once_cell::unsync::OnceCell;
use serde::{Deserialize, Serialize};
use snafu::prelude::*;
use alloc::borrow::ToOwned;

use alloc::{
    borrow::Cow,
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
    prelude::Framework,
    settings::{FILE_STORE_MAX_DIRS, FILE_STORE_MAX_FILES},
    term_error, term_info,
};

use crate::{
    bambu::{FilamentInfo, TagInformation},
    csvdb::{CsvDb, CsvDbError, CsvDbId},
};

#[derive(Snafu, Debug)]
pub enum InternalError {
    TagIdTooLong,
    BadTagId,
    BadId,
}

#[derive(Snafu, Debug)]
pub enum StoreError {
    #[snafu(display("Too many store operations pending"))]
    TooManyOps,

    #[snafu(display("Error deleting spool: {source}"))]
    CsvDbError { source: CsvDbError },

    #[snafu(display("Internal store software logic error"))]
    InternalError,

    #[snafu(display("Record not found"))]
    NotFound { id: String },

    #[snafu(display("Can't access databse (SD Card Installed?)"))]
    NoCsvDb,
}

#[allow(clippy::enum_variant_names, dead_code)]
#[derive(Debug)]
pub enum WeightStoreDirective {
    ProvidedCurrentWeight(i32),
    UseStoreCurrentWeight,
    ClearCurrentWeight,
}

#[allow(dead_code)]
#[derive(Debug)]
pub enum TagFileDirective {
    SkipWrite,
    AlwaysWrite,
    WriteIfMissing,
}

#[derive(Debug)]
pub enum FieldsOverrideDirective {
    TagOverride,
    StoreOverride,
}

#[derive(Debug)]
pub enum StoreOp {
    WriteTag {
        tag_info: TagInformation,
        tag_file: TagFileDirective,
        weight: WeightStoreDirective,
        cookie: Box<dyn AnyClone>,
        fields: FieldsOverrideDirective,
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
        });
        framework.borrow().spawner.spawn(store_task(framework.clone(), store.clone())).ok();
        store
    }

    pub fn subscribe(&self, observer: alloc::rc::Weak<RefCell<dyn StoreObserver>>) {
        self.observers.borrow_mut().push(observer);
    }

    pub fn notify_tag_stored(&self, result: Result<Option<&str>, &str>, cookie: Box<dyn AnyClone>) {
        for weak_observer in self.observers.borrow().iter() {
            let observer = weak_observer.upgrade().unwrap();
            observer
                .borrow_mut()
                .on_tag_stored(result.map(|s| s.map(str::to_owned)).map_err(|e| e.to_string()), cookie.clone());
        }
    }

    pub fn try_send_op(&self, op: StoreOp) -> Result<(), StoreError> {
        self.requests_channel.try_send(op).map_err(|_| StoreError::TooManyOps)
    }

    pub fn is_available(&self) -> bool {
        true
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
                if let Ok(spool_rec_ext_file_path) = spool_rec_ext_file_path(&deleted_record) {
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
                        added_time: current_record.added_time,
                        encode_time: current_record.encode_time,
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
}

#[embassy_executor::task] // up to two printers in parallel
pub async fn store_task(framework: Rc<RefCell<Framework>>, store: Rc<Store>) {
    let db_available;
    {
        debug!("Strted store_task");
        let file_store = framework.borrow().file_store();
        match CsvDb::<SpoolRecord, _, FILE_STORE_MAX_DIRS, FILE_STORE_MAX_FILES>::new(file_store.clone(), "/store/spools", 1024, 200, true, true)
            .await
        {
            Ok(db) => {
                store
                    .spools_db
                    .set(db)
                    .map_err(|_e| "Fatal Internal Error: Can't assign spools_db to once_cell?")
                    .unwrap();
                term_info!("Opened spools database");
                db_available = true;
            }
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

    let receiver = store.requests_channel.receiver();
    loop {
        match receiver.receive().await {
            StoreOp::WriteTag {
                tag_info,
                tag_file,
                weight,
                fields,
                cookie,
            } => {
                if db_available {
                    let filament_info = tag_info.filament.unwrap_or(FilamentInfo::new());
                    let tag_id_hex = tag_id_hex(tag_info.tag_id.as_ref().unwrap());
                    let tag_id_already_exist;
                    let mut existing_record_current_weight = None;
                    let mut existing_record_current_note = "".to_string();
                    let mut use_spool_id = tag_info.id.unwrap_or_default();

                    // debug!(">>>> on entry use_spool_id={use_spool_id}");

                    if let Some(spools_db) = store.spools_db.get() {
                        if !use_spool_id.is_empty() && !spools_db.records.borrow().contains_key(&use_spool_id) {
                            error!("Software Logic Error, encoded from id that isn't found");
                            store.notify_tag_stored(Err(&format!("Internal Software Error, encoded ID not found {use_spool_id}")), cookie);
                            continue;
                        }
                        let spool_id_from_tag_id_clone = store.tag_id_index.borrow().get(&tag_id_hex).cloned();
                        if let Some(spool_id_from_tag_id) = spool_id_from_tag_id_clone {
                            // debug!(">>>>> Found tag_id {spool_id_from_tag_id}");
                            if !use_spool_id.is_empty() && spool_id_from_tag_id != use_spool_id {
                                // debug!(">>>> spool_id_from_tag_id ({spool_id_from_tag_id} != use_spool_id {use_spool_id}");
                                // This is a case where encoding using an inventory spool-id when the tag_id is already in use
                                // in such case we need to 'strikeout' the tag_id (add a "-" to the beginning)
                                // practically this means tag_id doen't exist after we are done here
                                let mut record_to_strike = {
                                    let records_borrow = spools_db.records.borrow();
                                    let record_wrapper = records_borrow.get(&spool_id_from_tag_id).unwrap();
                                    record_wrapper.data.clone()
                                };
                                record_to_strike.tag_id = format!("-{}", record_to_strike.tag_id);
                                let _ = spools_db.insert(record_to_strike).await;
                                store.tag_id_index.borrow_mut().remove(&tag_id_hex);
                                tag_id_already_exist = false;
                            } else if let Some(current_rec) = spools_db.records.borrow().get(&spool_id_from_tag_id) {
                                // debug!(
                                //     ">>>> spool_id_from_tag_id ({spool_id_from_tag_id} == use_spool_id {use_spool_id}, and gained access to the record"
                                // );
                                // get the record, should exist if got here, if not fatal error
                                existing_record_current_weight = current_rec.data.weight_current;
                                existing_record_current_note = current_rec.data.note.clone();
                                use_spool_id = current_rec.data.id.clone();
                                tag_id_already_exist = true;
                            } else {
                                error!("Fatal Error: Internal error in tag_id to spool_id mapping, tag exist but not found");
                                store.notify_tag_stored(Err("Internal software error, tag exists but not found"), cookie);
                                continue;
                            }
                        } else {
                            // debug!(">>>>> tag_id doesn't exist (1)");
                            tag_id_already_exist = false;
                        }
                    } else {
                        // debug!(">>>>> tag_id doesn't exist (2)");
                        tag_id_already_exist = false;
                    }
                    let added_new_record;
                    if use_spool_id.is_empty() {
                        // debug!(">>>>> use_spool_id is empty");
                        // don't change yet the last_spool_id in case store fail
                        use_spool_id = (*store.last_spool_id.borrow() + 1).to_string();
                        added_new_record = true;
                    } else {
                        // debug!(">>>>> use_spool_id is NOT empty");
                        added_new_record = false;
                    }

                    let mut spool_rec = SpoolRecord {
                        id: use_spool_id.clone(),
                        tag_id: tag_id_hex.clone(),
                        material_type: filament_info.tray_type,
                        material_subtype: tag_info.filament_subtype.unwrap_or_default(),
                        color_name: tag_info.color_name.unwrap_or_default(),
                        color_code: filament_info.tray_color,
                        note: tag_info.note.unwrap_or_default(),
                        brand: tag_info.brand.unwrap_or_default(),
                        weight_advertised: tag_info.weight_advertised,
                        weight_core: tag_info.weight_core,
                        weight_new: tag_info.weight_new,
                        weight_current: None,
                        slicer_filament: filament_info.tray_info_idx,
                        added_time: None,
                        encode_time: tag_info.encode_time,
                    };
                    if let Some(spools_db) = store.spools_db.get() {
                        spool_rec.weight_current = match weight {
                            WeightStoreDirective::ProvidedCurrentWeight(weight_current) => Some(weight_current),
                            WeightStoreDirective::UseStoreCurrentWeight => {
                                if tag_id_already_exist {
                                    existing_record_current_weight
                                } else {
                                    None
                                }
                            }
                            WeightStoreDirective::ClearCurrentWeight => None,
                        };

                        if tag_id_already_exist {
                            match fields {
                                FieldsOverrideDirective::TagOverride => (),
                                FieldsOverrideDirective::StoreOverride => spool_rec.note = existing_record_current_note,
                            }
                        }

                        if !added_new_record {
                            if let Some(current_rec) = spools_db.records.borrow().get(&use_spool_id) {
                                spool_rec.added_time = current_rec.data.added_time;
                            }
                        }

                        // debug!(">>>> Storing {spool_rec:?}");

                        match spools_db.insert(spool_rec.clone()).await {
                            Ok(true) => {
                                info!("Stored tag to spools database");
                                store.notify_tag_stored(Ok(Some(&spool_rec.id)), cookie.clone());
                            }
                            Ok(false) => {
                                info!("Stored tag to spools database, but no change");
                                store.notify_tag_stored(Ok(Some(&spool_rec.id)), cookie.clone());
                            }
                            Err(e) => {
                                error!("Error storing record to spools database {e}");
                                store.notify_tag_stored(Err(&format!("Failed to store Tag : {e}")), cookie);
                                continue;
                            }
                        }
                    } else {
                        error!("Store for tags not available, SD card removed?");
                        store.notify_tag_stored(Err("Store for tags not available, SD card removed?"), cookie);
                        continue;
                    }

                    // Store of record succeeded and case of new record added, so need to update index and last_spool_id
                    if added_new_record {
                        *store.last_spool_id.borrow_mut() = use_spool_id.parse().unwrap();
                    }

                    if !tag_id_already_exist {
                        // add to index if required
                        store.tag_id_index.borrow_mut().insert(tag_id_hex, use_spool_id);
                    }

                    //////////////////////////////////////////////////////////////////////////////////////////
                    // Write tag file (or not) ///////////////////////////////////////////////////////////////
                    //////////////////////////////////////////////////////////////////////////////////////////
                    if !matches!(tag_file, TagFileDirective::SkipWrite) {
                        if let Some(tag_id) = &tag_info.tag_id.as_ref() {
                            if tag_id.len() <= 7 {
                                if let Ok(spool_rec_ext_file_path) = spool_rec_ext_file_path(&spool_rec) {
                                    let file_store = framework.borrow().file_store();
                                    let mut file_store = file_store.lock().await;
                                    let write_only_if_missing = match tag_file {
                                        TagFileDirective::SkipWrite => panic!("Critical Software Bug"),
                                        TagFileDirective::AlwaysWrite => false,
                                        TagFileDirective::WriteIfMissing => true,
                                    };
                                    let spool_rec_ext = SpoolRecordExt {
                                        tag: Some(Cow::Borrowed(&tag_info.origin_descriptor)),
                                    };
                                    match serde_json::to_string(&spool_rec_ext) {
                                        Ok(s) => match file_store.write_file_str(&spool_rec_ext_file_path, 0, &s, write_only_if_missing).await {
                                            Ok(wrote) => {
                                                if wrote {
                                                    info!("Stored tag {tag_id:?} information to file {spool_rec_ext_file_path}");
                                                } else {
                                                    info!("Skipped store tag {tag_id:?} information to file {spool_rec_ext_file_path}, file already exists");
                                                }
                                            }
                                            Err(err) => {
                                                error!("Error writing tag file to {spool_rec_ext_file_path} : {err}");
                                                store.notify_tag_stored(
                                                    Err("Inventory updated, but failed writing extended info (1), check logs"),
                                                    cookie,
                                                );
                                                continue;
                                            }
                                        },
                                        Err(e) => {
                                            error!("Error serializing tag information to store: {e}");
                                            store.notify_tag_stored(
                                                Err("Inventory updated, but failed writing extended info (2), check logs"),
                                                cookie,
                                            );
                                            continue;
                                        }
                                    }
                                } else {
                                    continue;
                                }
                            } else {
                                error!("Can't save tag_id longer than 7 bytes");
                                store.notify_tag_stored(Err("Inventory updated, but failed writing extended info (3), check logs"), cookie);
                                continue;
                            }
                        }
                    }
                } else {
                    store.notify_tag_stored(Ok(None), cookie.clone());
                }
            }
        }
    }
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

#[derive(Serialize, Deserialize)]
struct SpoolRecordExt<'a> {
    tag: Option<Cow<'a, String>>,
}

impl CsvDbId for SpoolRecord {
    fn id(&self) -> &String {
        &self.id
    }
}

pub trait StoreObserver {
    fn on_tag_stored(&mut self, result: Result<Option<String>, String>, cookie: Box<dyn AnyClone>); // String result is id of stored tag
}

fn tag_id_hex(tag_id: &[u8]) -> String {
    hex::encode_upper(tag_id)
}

fn spool_rec_ext_file_path(ext_rec: &SpoolRecord) -> Result<String, InternalError> {
    if let Ok(id_num) = ext_rec.id.parse::<i32>() {
        let folder_num = ((id_num / 16) % 16) + 1;
        let file_path = format!("/store/spools.ext/{folder_num}/{id_num}.jsn");
        Ok(file_path)
    } else {
        Err(InternalError::BadId)
    }
}

pub fn store_safe_time_now() -> Option<i32> {
    Instant::now().to_date_time().map(|date_time| date_time.timestamp() as i32)
}

// const FAT_CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ123456789"; // 35 chars
//
// fn encode_to_charset(input: &[u8], charset: &[u8]) -> String {
//     let base = charset.len() as u16;
//     assert!((2..=256).contains(&base));
//
//     let mut bytes = input.to_vec(); // cloned internally
//     let mut output = Vec::new();
//
//     while bytes.iter().any(|&b| b != 0) {
//         let mut rem = 0u16;
//         for b in &mut bytes {
//             let val = (rem << 8) | *b as u16;
//             *b = (val / base) as u8;
//             rem = val % base;
//         }
//         output.push(charset[rem as usize] as char);
//     }
//
//     let min_len = ((input.len() * 8) as f64 / (base as f64).log2()).ceil() as usize;
//     while output.len() < min_len {
//         output.push(charset[0] as char);
//     }
//
//     output.reverse();
//     output.into_iter().collect()
// }
// pub fn fnv1a_hash(data: &[u8]) -> u64 {
//     let mut hash = 0xcbf29ce484222325; // FNV offset basis
//     for byte in data.iter() {
//         hash ^= *byte as u64;
//         hash = hash.wrapping_mul(0x100000001b3);
//     }
//     hash
// }

// #[allow(dead_code)]
// fn decode_from_charset(s: &str, charset: &[u8]) -> Option<Vec<u8>> {
//     let base = charset.len() as u32;
//     assert!((2..=256).contains(&base));
//
//     // Build char lookup table
//     let mut char_to_val = [None; 256];
//     for (i, &c) in charset.iter().enumerate() {
//         char_to_val[c as usize] = Some(i as u32);
//     }
//
//     // Infer original byte length
//     let bit_len = (s.len() as f64) * (base as f64).log2();
//     let byte_len = (bit_len / 8.0).floor() as usize;
//
//     let mut result = alloc::vec![0u8; byte_len];
//
//     for ch in s.bytes() {
//         let digit = char_to_val[ch as usize]?; // Invalid char
//         let mut carry = digit;
//
//         for byte in result.iter_mut().rev() {
//             let val = (*byte as u32) * base + carry;
//             *byte = (val & 0xFF) as u8;
//             carry = val >> 8;
//         }
//
//         if carry != 0 {
//             return None; // Overflow
//         }
//     }
//
//     Some(result)
// }
