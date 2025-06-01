use core::{any::Any, cell::RefCell};
use once_cell::unsync::OnceCell;
use serde::{Deserialize, Serialize};
use snafu::prelude::*;

use alloc::{boxed::Box, format, rc::Rc, string::{String, ToString}, vec::Vec};
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, channel::Channel};
use framework::{
    debug, error, info, mk_static,
    prelude::Framework,
    settings::{FILE_STORE_MAX_DIRS, FILE_STORE_MAX_FILES},
    warn,
};

use crate::{
    bambu::{FilamentInfo, TagInformation},
    csvdb::{CsvDb, CsvDbError, CsvDbId},
};

#[derive(Snafu, Debug)]
pub enum StoreError {
    #[snafu(display("Too many store operations pending"))]
    TooManyOps,
}

#[derive(Debug)]
pub enum StoreOp {
    WriteTag { tag_info: TagInformation, weight: Option<i32>, cookie: Box<dyn AnyClone> },
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
    T: Cookie // Any + Clone  + core::fmt::Debug + 'static,
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
    observers: RefCell<Vec<alloc::rc::Weak<RefCell<dyn StoreObserver>>>>,
    pub requests_channel: &'static StoreRequestsChannel,
    // TODO: make spools_db mutext or something that doesn't need borrow
    // Think if need to make the entire store under mutex (if there are several related dbs could case issues)
    pub spools_db: OnceCell<CsvDb<SpoolRecord, TheSpi, 20, 5>>,
}

impl Store {
    pub fn new(framework: Rc<RefCell<Framework>>) -> Rc<Store> {
        let requests_channel = mk_static!(StoreRequestsChannel, StoreRequestsChannel::new());
        let store = Rc::new(Self {
            observers: RefCell::new(Vec::new()),
            requests_channel,
            spools_db: OnceCell::new(),
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
}

#[embassy_executor::task] // up to two printers in parallel
pub async fn store_task(framework: Rc<RefCell<Framework>>, store: Rc<Store>) {
    {
        debug!("Strted store_task");
        let file_store = framework.borrow().file_store();
        match CsvDb::<SpoolRecord, _, FILE_STORE_MAX_DIRS, FILE_STORE_MAX_FILES>::new(file_store.clone(), "/store/spools", 128, 200).await {
            Ok(db) => {
                store
                    .spools_db
                    .set(db)
                    .map_err(|_e| "Fatal Internal Error: Can't assign spools_db to once_cell?")
                    .unwrap();
                info!("Opened spools database");
            }
            Err(e) => {
                warn!("Failed to open spools database : {e}");
                return;
            }
        }
    }
    let receiver = store.requests_channel.receiver();
    loop {
        match receiver.receive().await {
            StoreOp::WriteTag { tag_info, weight, cookie } => {
                if tag_info.tag_id.is_some() {
                    let filament_info = tag_info.filament.unwrap_or(FilamentInfo::new());
                    let mut spool_rec = SpoolRecord {
                        tag_id: tag_info.tag_id.unwrap(),
                        material_type: filament_info.tray_type,
                        material_subtype: tag_info.filament_subtype.unwrap_or_default(),
                        color_name: tag_info.color_name.unwrap_or_default(),
                        color_code: filament_info.tray_color,
                        note: tag_info.note.unwrap_or_default(),
                        brand: tag_info.brand.unwrap_or_default(),
                        weight_left: weight,
                    };
                    if let Some(spools_db) = store.spools_db.get() {
                        if weight.is_none() {
                            if let Some(current_rec) = spools_db.records.borrow().get(&spool_rec.tag_id) {
                                spool_rec.weight_left = current_rec.data.weight_left;
                            }
                        }
                        match spools_db.insert(spool_rec).await {
                            Ok(true) => {
                                info!("Stored tag to spools database");
                                store.notify_tag_stored(Ok(()), cookie);
                            }
                            Ok(false) => {
                                info!("Stored tag to spools database, but no change");
                                store.notify_tag_stored(Ok(()), cookie);
                            }
                            Err(e) => {
                                error!("Error storing record to spools database {e}");
                                store.notify_tag_stored(Err(&format!("Failed to store Tag : {e}")), cookie);
                            }
                        }
                        info!("{:?}", spools_db.records.borrow());
                    } else {
                        store.notify_tag_stored(Err("Store for tags not available, SD card installed?"), cookie);
                    }
                }
            }
        }
    }
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct SpoolRecord {
    pub tag_id: String,           // 14 (7*2)
    pub material_type: String,    // 10
    pub material_subtype: String, // 10
    pub color_name: String,       // 10
    pub color_code: String,       // 8
    pub note: String,             // 40
    pub brand: String,            // 30
    pub weight_left: Option<i32>, // 4
}

impl CsvDbId for SpoolRecord {
    fn id(&self) -> &String {
        &self.tag_id
    }
}

pub trait StoreObserver {
    fn on_tag_stored(&mut self, result: Result<(), String>, cookie:  Box<dyn AnyClone>);
}
