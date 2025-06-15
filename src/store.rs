use core::{any::Any, cell::RefCell};
use hashbrown::HashMap;
use num_traits::Float;
use once_cell::unsync::OnceCell;
use serde::{Deserialize, Serialize};
use snafu::prelude::*;

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
   TagIdTooLong 
}

#[derive(Snafu, Debug)]
pub enum StoreError {
    #[snafu(display("Too many store operations pending"))]
    TooManyOps,

    #[snafu(display("Error deleting spool: {source}"))]
    DeleteSpoolError { source: CsvDbError },
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
pub enum StoreOp {
    WriteTag {
        tag_info: TagInformation,
        tag_file: TagFileDirective,
        weight: WeightStoreDirective,
        cookie: Box<dyn AnyClone>,
    },
}

#[derive(Serialize, Deserialize)]
struct TagFile<'a> {
    tag: Cow<'a, String>,
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

    pub fn notify_tag_stored(&self, result: Result<(), &str>, cookie: Box<dyn AnyClone>) {
        for weak_observer in self.observers.borrow().iter() {
            let observer = weak_observer.upgrade().unwrap();
            observer.borrow_mut().on_tag_stored(result.map_err(|e| e.to_string()), cookie.clone());
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
            delete_res.context(DeleteSpoolSnafu)?
        } else {
            None
        };

        if let Some(deleted_record) = deleted_record {
            if let Ok(tag_id) = hex::decode(deleted_record.tag_id) {
                if let Ok(tag_file_path) = tag_file_path(&tag_id) {
                    let file_store = self.framework.borrow().file_store();
                    let mut file_store = file_store.lock().await;
                    let _ = file_store.delete_file(&tag_file_path).await;
                }
            }
        }
        Ok(())
    }
}

#[embassy_executor::task] // up to two printers in parallel
pub async fn store_task(framework: Rc<RefCell<Framework>>, store: Rc<Store>) {
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
            }
            Err(e) => {
                term_error!("Failed to open spools database : {}", e);
                return;
            }
        }
    }

    // find largest_id in list, would be better if we persisted that

    let mut largest_id = 0;
    if let Some(spools_db) = store.spools_db.get() {
        let records = spools_db.records.borrow();
        for record in records.iter() {
            if let Ok(id) = record.1.data.id.parse::<i32>() {
                if !record.1.data.tag_id.is_empty() {
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
    *store.last_spool_id.borrow_mut() = largest_id;

    let receiver = store.requests_channel.receiver();
    loop {
        match receiver.receive().await {
            StoreOp::WriteTag {
                tag_info,
                tag_file,
                weight,
                cookie,
            } => {
                if tag_info.tag_id.is_some() {
                    let filament_info = tag_info.filament.unwrap_or(FilamentInfo::new());
                    let tag_id_hex = hex::encode_upper(tag_info.tag_id.as_ref().unwrap());
                    let mut tag_id_already_exist = false;
                    let mut existing_record_current_weight = None;
                    let mut use_spool_id = String::new();

                    if let Some(spools_db) = store.spools_db.get() {
                        // get access to db
                        if let Some(spool_id) = store.tag_id_index.borrow().get(&tag_id_hex) {
                            // search if tag_id exists (in mapping from tag to id)
                            tag_id_already_exist = true;
                            if let Some(current_rec) = spools_db.records.borrow().get(spool_id) {
                                // get the record, should exist if got here, if not fatal error
                                existing_record_current_weight = current_rec.data.weight_current;
                                use_spool_id = current_rec.data.id.clone();
                            } else {
                                error!("Fatal Error: Internal error in tag_id to spool_id mapping, tag exist but not found");
                                store.notify_tag_stored(Err("Internal software error managing store"), cookie);
                                continue;
                            }
                        }
                    }
                    if !tag_id_already_exist {
                        // don't change yet the last_spool_id in case store fail
                        use_spool_id = (*store.last_spool_id.borrow() + 1).to_string();
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
                        match spools_db.insert(spool_rec).await {
                            Ok(true) => {
                                info!("Stored tag to spools database");
                                store.notify_tag_stored(Ok(()), cookie.clone());
                            }
                            Ok(false) => {
                                info!("Stored tag to spools database, but no change");
                                store.notify_tag_stored(Ok(()), cookie.clone());
                            }
                            Err(e) => {
                                error!("Error storing record to spools database {e}");
                                store.notify_tag_stored(Err(&format!("Failed to store Tag : {e}")), cookie);
                                continue;
                            }
                        }
                        info!("{:?}", spools_db.records.borrow());
                    } else {
                        store.notify_tag_stored(Err("Store for tags not available, SD card removed?"), cookie);
                        continue;
                    }
                    // Store of record succeeded, so need to update index and last_spool_id
                    if !tag_id_already_exist {
                        *store.last_spool_id.borrow_mut() = use_spool_id.parse().unwrap();
                        store.tag_id_index.borrow_mut().insert(tag_id_hex, use_spool_id);
                    }
                    // Write tag file (or not)
                    if !matches!(tag_file, TagFileDirective::SkipWrite) {
                        if let Some(tag_id) = &tag_info.tag_id.as_ref() {
                            if tag_id.len() <= 7 {
                                if let Ok(tag_file_path) = tag_file_path(tag_id) {
                                    let file_store = framework.borrow().file_store();
                                    let mut file_store = file_store.lock().await;
                                    let write_only_if_missing = match tag_file {
                                        TagFileDirective::SkipWrite => panic!("Critical Software Bug"),
                                        TagFileDirective::AlwaysWrite => false,
                                        TagFileDirective::WriteIfMissing => true,
                                    };
                                    let tag_file_data = TagFile {
                                        tag: Cow::Borrowed(&tag_info.origin_descriptor),
                                    };
                                    match serde_json::to_string(&tag_file_data) {
                                        Ok(s) => match file_store.write_file_str(&tag_file_path, 0, &s, write_only_if_missing).await {
                                            Ok(wrote) => {
                                                if wrote {
                                                    info!("Stored tag {tag_id:?} information to file {tag_file_path}");
                                                } else {
                                                    info!("Skipped store tag {tag_id:?} information to file {tag_file_path}, file already exists");
                                                }
                                            }
                                            Err(err) => {
                                                error!("Error writing tag file to {tag_file_path} : {err}");
                                                store.notify_tag_stored(Err("Error writing tag file (1), check logs for more details"), cookie);
                                                continue;
                                            }
                                        },
                                        Err(e) => {
                                            error!("Error serializing tag information to store: {e}");
                                            store.notify_tag_stored(Err("Error writing tag file (2), check logs for more details"), cookie);
                                            continue;
                                        }
                                    }
                                } else {
                                    continue;
                                } 
                            } else {
                                error!("Can't save tag_id longer than 7 bytes");
                                store.notify_tag_stored(Err("Error writing tag file (3), check logs for more details"), cookie);
                                continue;
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct SpoolRecord {
    pub id: String,
    pub tag_id: String,                 // 14 (7*2)
    pub material_type: String,          // 10
    pub material_subtype: String,       // 10
    pub color_name: String,             // 10
    pub color_code: String,             // 8
    pub note: String,                   // 40
    pub brand: String,                  // 30
    pub weight_advertised: Option<i32>, // 4
    pub weight_core: Option<i32>,       // 4
    pub weight_new: Option<i32>,        // 4
    pub weight_current: Option<i32>,    // 4
}

impl CsvDbId for SpoolRecord {
    fn id(&self) -> &String {
        &self.id
    }
}

pub trait StoreObserver {
    fn on_tag_stored(&mut self, result: Result<(), String>, cookie: Box<dyn AnyClone>);
}

const FAT_CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ123456789"; // 35 chars

fn encode_to_charset(input: &[u8], charset: &[u8]) -> String {
    let base = charset.len() as u16;
    assert!((2..=256).contains(&base));

    let mut bytes = input.to_vec(); // cloned internally
    let mut output = Vec::new();

    while bytes.iter().any(|&b| b != 0) {
        let mut rem = 0u16;
        for b in &mut bytes {
            let val = (rem << 8) | *b as u16;
            *b = (val / base) as u8;
            rem = val % base;
        }
        output.push(charset[rem as usize] as char);
    }

    let min_len = ((input.len() * 8) as f64 / (base as f64).log2()).ceil() as usize;
    while output.len() < min_len {
        output.push(charset[0] as char);
    }

    output.reverse();
    output.into_iter().collect()
}

#[allow(dead_code)]
fn decode_from_charset(s: &str, charset: &[u8]) -> Option<Vec<u8>> {
    let base = charset.len() as u32;
    assert!((2..=256).contains(&base));

    // Build char lookup table
    let mut char_to_val = [None; 256];
    for (i, &c) in charset.iter().enumerate() {
        char_to_val[c as usize] = Some(i as u32);
    }

    // Infer original byte length
    let bit_len = (s.len() as f64) * (base as f64).log2();
    let byte_len = (bit_len / 8.0).floor() as usize;

    let mut result = alloc::vec![0u8; byte_len];

    for ch in s.bytes() {
        let digit = char_to_val[ch as usize]?; // Invalid char
        let mut carry = digit;

        for byte in result.iter_mut().rev() {
            let val = (*byte as u32) * base + carry;
            *byte = (val & 0xFF) as u8;
            carry = val >> 8;
        }

        if carry != 0 {
            return None; // Overflow
        }
    }

    Some(result)
}

fn tag_file_path(tag_id: &[u8]) -> Result<String, InternalError> {
    let encoded_tag_id = encode_to_charset(tag_id, FAT_CHARSET);
    if encoded_tag_id.len() > 11 {
        return TagIdTooLongSnafu.fail();
    }
    let tag_bucket = fnv1a_hash(tag_id) % 16;
    Ok(format!(
        "/store/tags/{:04X}/{}.{}",
        tag_bucket,
        &encoded_tag_id[..8],
        &encoded_tag_id[8..11]
    ))
}

pub fn fnv1a_hash(data: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325; // FNV offset basis
    for byte in data.iter() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}
