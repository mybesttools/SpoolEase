use core::cell::RefCell;
use core::cmp::max;
use core::ops::{Deref, DerefMut};

use alloc::boxed::Box;
use alloc::string::String;
use alloc::{
    format,
    rc::{Rc, Weak},
    string::ToString,
    vec::Vec,
};
use embassy_executor::raw::TaskStorage;
use embassy_net::Stack;
use embassy_time::{Instant, Timer};
use embedded_hal_bus::spi::ExclusiveDevice;
use hashbrown::HashMap;
use serde::{Deserialize, Serialize};
use shared::gcode_analysis::FilamentUsageEntry;
use shared::gcode_analysis_task::{
    fetch_gcode_analysis_task, FilamentUsage, GcodeAnalysisNotification, GcodeAnalysisNotificationChannel, GcodeAnalysisRequest,
    GcodeAnalysisRequestChannel, GcodeAnalyzerObserver,
};
use slint::{ComponentHandle, Model, SharedString, ToSharedString};

use framework::prelude::*;
use framework::{
    framework::{FrameworkObserver, WebConfigMode},
    terminal::{self, term_mut, TerminalObserver},
};

use crate::app::{EncodeRequest, FilamentInfoMode};
use crate::app_config::{BASE_FILAMENTS, FILAMENT_BRAND_NAMES, SPOOLS_CATALOG};
use crate::bambu::bambu_print::{GcodeAnalysis, PrintProject};
use crate::bambu::{FilamentInfo, Tray, TrayBits};
use crate::color_utils::get_color_name;
use crate::filament_staging::{self, StagingOrigin};
use crate::spool_scale::{self, ScaleWeight, SpoolScaleObserver};
use crate::ssdp::{ssdp_task, SSDPPubSubChannel};
use crate::store::{store_safe_time_now, AnyClone, Cookie, Store, StoreObserver, StoreOp, TagOperation};
use crate::web_app::EncodeInfoDTO;
use crate::{
    app_config::AppConfig,
    bambu::{self, BambuPrinter, BambuPrinterObserver, TagInformation, TrayState},
    filament_staging::FilamentStaging,
};
use shared::spool_tag::{self, SpoolTagObserver, Status};

struct PrinterUiState {
    curr_ams: Option<i32>,
}
pub struct ViewModel {
    // Framework
    stack: Stack<'static>,
    ui_weak: slint::Weak<crate::app::AppWindow>,
    view_model: Option<Rc<RefCell<Self>>>,
    framework: Rc<RefCell<Framework>>,
    _terminal_view_model: Rc<RefCell<TerminalViewModel>>,
    // Application
    #[allow(dead_code)]
    app_config: Rc<RefCell<AppConfig>>,
    bambu_printer_model: SelectedPrinter,
    spool_tag_model: Rc<RefCell<spool_tag::SpoolTag>>,
    spool_scale_model: Rc<RefCell<spool_scale::SpoolScale>>,
    filament_staging: Rc<RefCell<FilamentStaging>>,
    printers_view_state: HashMap<String, PrinterUiState>,

    cores_list_vec_rc: slint::ModelRc<crate::app::SelectorOption>,
    spools_cores_weights: HashMap<i32, i32>,
    spools_cores_filter: String,
    pub store: Rc<Store>,
    encode_from_blank: Option<TagInformation>,
    gcode_analysis_request_channel: Rc<GcodeAnalysisRequestChannel>,
    gcode_analysis_notification_channel: Rc<GcodeAnalysisNotificationChannel>,
    gcode_last_job_number: i32,
}

#[derive(Serialize, Deserialize, Clone)]
struct EncodeCookie {
    scale_weight: ScaleWeight,
    spool_id: String,
}

impl ViewModel {
    pub fn new(
        // Framework
        stack: Stack<'static>,
        ui_weak: slint::Weak<crate::app::AppWindow>,
        framework: Rc<RefCell<Framework>>,
        // Application
        app_config: Rc<RefCell<AppConfig>>,
        // bambu_printer_model: Rc<RefCell<bambu::BambuPrinter>>,
        spi_device: ExclusiveDevice<esp_hal::spi::master::SpiDmaBus<'static, esp_hal::Async>, esp_hal::gpio::Output<'static>, embassy_time::Delay>,
        irq: esp_hal::gpio::Input<'static>,
    ) -> Rc<RefCell<ViewModel>> {
        let spawner = framework.borrow().spawner;
        // Setup Terminal
        let terminal_view_model = Rc::new(RefCell::new(TerminalViewModel { ui_weak: ui_weak.clone() }));
        let trait_for_terminal_rc: Rc<RefCell<dyn terminal::TerminalObserver>> = terminal_view_model.clone();
        let trait_for_terminal_weak: Weak<RefCell<dyn terminal::TerminalObserver>> = Rc::downgrade(&trait_for_terminal_rc);
        term_mut().subscribe(trait_for_terminal_weak);

        // Setup empty printers
        let set_of_printers: Vec<Rc<RefCell<BambuPrinter>>> = Vec::new();
        let selected_printer = SelectedPrinter::new(set_of_printers, 0);

        // Initialize SpoolTag
        let spool_tag_model = spool_tag::init(spi_device, irq, spawner);

        // Initialize ssdp
        let ssdp_pub_sub = mk_static!(SSDPPubSubChannel, SSDPPubSubChannel::new());
        let task = Box::leak(Box::new(TaskStorage::new())).spawn(|| ssdp_task(framework.clone(), ssdp_pub_sub));
        spawner.spawn(task).ok();
        // spawner.spawn(ssdp_task(framework.clone(), ssdp_pub_sub)).ok();

        // Initialize store
        let store = Store::new(framework.clone());

        // Initialize spool_scale_model
        let spool_scale_model = crate::spool_scale::init(framework.clone(), app_config.clone(), stack, spawner, ssdp_pub_sub);

        // Prepare an empty spool weights lists, later we'll replace it
        let spools_cores_weights: HashMap<i32, i32> = HashMap::with_capacity(300);
        let selector_options_vec: slint::VecModel<crate::app::SelectorOption> = slint::VecModel::default();
        let selector_options_vec_rc = slint::ModelRc::from(Rc::new(selector_options_vec));

        let gcode_analysis_request_channel = Rc::new(GcodeAnalysisRequestChannel::new());
        let gcode_analysis_notification_channel = Rc::new(GcodeAnalysisNotificationChannel::new());

        // Create the ViewModel
        let view_model = ViewModel {
            // Framework
            stack,
            ui_weak: ui_weak.clone(),
            view_model: None,
            framework: framework.clone(),
            _terminal_view_model: terminal_view_model, // used by Terminal with weak reference, hold it so it won't be released
            // Application
            bambu_printer_model: selected_printer,
            spool_tag_model: spool_tag_model.clone(),
            spool_scale_model: spool_scale_model.clone(),
            app_config: app_config.clone(),
            filament_staging: Rc::new(RefCell::new(FilamentStaging::new(store.clone()))),
            printers_view_state: HashMap::new(),
            cores_list_vec_rc: selector_options_vec_rc,
            spools_cores_weights,
            spools_cores_filter: String::new(),
            store,
            encode_from_blank: None,
            gcode_analysis_request_channel,
            gcode_analysis_notification_channel,
            gcode_last_job_number: 0,
        };
        let view_model_rc = Rc::new(RefCell::new(view_model));

        // hold a reference to itself to hand over to others, this is a 'memory leak' but object never gets destroyed so eaiser than weak reference
        view_model_rc.borrow_mut().view_model = Some(view_model_rc.clone());

        // Initialize
        view_model_rc.borrow_mut().init_framework_stuff();
        view_model_rc.borrow_mut().init_app_stuff(ssdp_pub_sub);

        // Done
        view_model_rc
    }

    pub fn init_framework_stuff(&mut self) {
        // Subscribe to rust structs framework events
        let trait_for_framework_rc: Rc<RefCell<dyn FrameworkObserver>> = self.view_model.as_ref().unwrap().clone();
        let trait_for_framework_weak: Weak<RefCell<dyn FrameworkObserver>> = Rc::downgrade(&trait_for_framework_rc);
        self.framework.borrow_mut().subscribe(trait_for_framework_weak);

        let ui = self.ui_weak.unwrap();

        // Initialize UI FrameworkState with framework information
        let ui_framework_state = ui.global::<crate::app::FrameworkState>();
        ui_framework_state.set_app_info(crate::app::AppInfo {
            name: env!("CARGO_PKG_NAME").into(),
            version: env!("CARGO_PKG_VERSION").into(),
        });

        // Register to UI (Slint) framework events (UI FrameworkBackend API's)
        let ui_framework_backend = ui.global::<crate::app::FrameworkBackend>();

        let framework = self.framework.clone();
        ui_framework_backend.on_reset_flash_wifi_credentials(move || {
            framework.borrow_mut().erase_stored_wifi_credentials();
            framework.borrow_mut().reset_device();
        });

        let framework = self.framework.clone();
        ui_framework_backend.on_reset_fixed_security_key(move || {
            let _ = framework.borrow_mut().set_fixed_key("");
        });

        let framework = self.framework.clone();
        let stack = self.stack;
        ui_framework_backend.on_start_web_config(move || {
            framework.borrow_mut().start_web_app(stack, WebConfigMode::STA);
        });

        let framework = self.framework.clone();
        ui_framework_backend.on_stop_web_config(move || {
            framework.borrow().stop_web_app();
        });

        let framework = self.framework.clone();
        ui_framework_backend.on_reset_device(move || {
            framework.borrow().reset_device();
        });

        let framework = self.framework.clone();
        ui_framework_backend.on_update_firmware_ota(move || {
            framework.borrow().update_firmware_ota();
        });
    }

    pub fn init_app_stuff(&mut self, ssdp_pub_sub: &'static SSDPPubSubChannel) {
        // Subscribe to rust spool_tag events
        let trait_for_spool_tag_rc: Rc<RefCell<dyn spool_tag::SpoolTagObserver>> = self.view_model.as_ref().unwrap().clone();
        let trait_for_spool_tag_weak: Weak<RefCell<dyn spool_tag::SpoolTagObserver>> = Rc::downgrade(&trait_for_spool_tag_rc);
        self.spool_tag_model.borrow_mut().subscribe(trait_for_spool_tag_weak);

        // Subscribe to rust spool_scale events
        let trait_for_spool_scale_rc: Rc<RefCell<dyn spool_scale::SpoolScaleObserver>> = self.view_model.as_ref().unwrap().clone();
        let trait_for_spool_scale_weak: Weak<RefCell<dyn spool_scale::SpoolScaleObserver>> = Rc::downgrade(&trait_for_spool_scale_rc);
        self.spool_scale_model.borrow_mut().subscribe(trait_for_spool_scale_weak);

        // Subscribe to rust store events
        // It's a bit different because store is Rc<Store> and not Rc<RefCell<Store>> due to Store different needs
        // ...I already don't remember those needs ... maybe not really needed anymore and originated in trying to solve something else there
        let trait_for_store_rc: Rc<RefCell<dyn StoreObserver>> = self.view_model.as_ref().unwrap().clone();
        let trait_for_store_weak: Weak<RefCell<dyn StoreObserver>> = Rc::downgrade(&trait_for_store_rc);
        self.store.subscribe(trait_for_store_weak);

        let ui = self.ui_weak.unwrap();
        let ui_app_backend = ui.global::<crate::app::AppBackend>();
        let ui_app_state = ui.global::<crate::app::AppState>();

        // Register to UI(Slint) app UI events
        let moved_filament_staging = self.filament_staging.clone();
        let moved_ui = self.ui_weak.clone();
        ui_app_backend.on_clear_staging(move || {
            moved_filament_staging.borrow_mut().clear();
            moved_ui.unwrap().global::<crate::app::AppState>().invoke_empty_spool_staging();
        });

        let moved_spool_tag = self.spool_tag_model.clone();
        ui_app_backend.on_read_tag_mode(move || {
            moved_spool_tag.borrow().read_tag();
        });

        let moved_spool_tag = self.spool_tag_model.clone();
        let moved_framework = self.framework.clone();
        let moved_app_config = self.app_config.clone();
        ui_app_backend.on_web_config_web_app(move || {
            moved_app_config.borrow_mut().set_redirect_web_to_config();
            let borrowed_framework = moved_framework.borrow();
            let web_config_ip_url = &borrowed_framework.web_config_ip_url;
            let web_config_key = &borrowed_framework.web_config_key;
            let full_web_config_url = format!("{web_config_ip_url}#sk={web_config_key}");
            moved_spool_tag.borrow().emulate_tag(&full_web_config_url);
        });

        let moved_spool_tag = self.spool_tag_model.clone();
        let moved_framework = self.framework.clone();
        let moved_app_config = self.app_config.clone();
        ui_app_backend.on_encode_web_app(move || {
            moved_app_config.borrow_mut().set_redirect_to_encode();
            let borrowed_framework = moved_framework.borrow();
            let web_config_ip_url = &borrowed_framework.web_config_ip_url;
            let web_config_key = &borrowed_framework.web_config_key;
            let full_web_config_url = format!("{web_config_ip_url}/encode#sk={web_config_key}");
            moved_spool_tag.borrow().emulate_tag(&full_web_config_url);
        });

        // Spool Scale
        let scale_available = if let Some(scale_config) = &self.app_config.borrow().configured_scale {
            scale_config.available
        } else {
            false
        };
        if !scale_available {
            ui_app_state.set_spool_scale_state(crate::app::SpoolScaleState::NotAvailable);
        }
        let moved_spool_scale_model = self.spool_scale_model.clone();
        ui_app_backend.on_calibrate_scale(move |weight| {
            moved_spool_scale_model.borrow_mut().calibrate(weight);
        });

        let moved_spool_scale_model = self.spool_scale_model.clone();
        ui_app_backend.on_get_connected_scale_info(move || {
            let connected_scale = &moved_spool_scale_model.borrow().connected_scale;
            if let Some(connected_scale) = connected_scale {
                let scale_name = match &connected_scale.0 {
                    Some(s) if !s.is_empty() => s.as_str(),
                    _ => "<Unnamed Scale/IP set w/o name>",
                };
                format!("{} - {}", connected_scale.1, scale_name).to_shared_string()
            } else {
                "<No Scale Connected>".to_shared_string()
            }
        });

        let moved_spool_scale_model = self.spool_scale_model.clone();
        ui_app_backend.on_get_available_scales_info(move || {
            let available_scales = &moved_spool_scale_model.borrow().available_scales;
            let mut available_scales_res = Vec::<SharedString>::new();

            for scale in available_scales {
                let scale_name = match &scale.0 {
                    Some(s) if !s.is_empty() => s.as_str(),
                    _ => "<Unnamed Scale>",
                };
                available_scales_res.push(format!("{} - {}", scale.1, scale_name).to_shared_string());
            }
            slint::ModelRc::new(slint::VecModel::from(available_scales_res))
        });

        // Initialize Printers ///////////////////////////

        let mut default_printer_set = false;
        let mut printer_number = 1; // starts from one and incremented for any printer
        let mut printer_index = 0; // starts from zero and incremented only on successful init and adding to array
        let mut available_printers: Vec<SharedString> = Vec::new();
        for printer_config in &self.app_config.borrow().configured_printers.printers {
            match bambu::init(
                self.framework.clone(),
                printer_number,
                printer_index,
                printer_config,
                self.app_config.clone(),
                ssdp_pub_sub,
            ) {
                Ok(bambu_printer_model) => {
                    self.bambu_printer_model.printers.push(bambu_printer_model.clone());
                    if !default_printer_set
                        && Some(&bambu_printer_model.borrow().printer_serial) == self.app_config.borrow().configured_default_printer.serial.as_ref()
                    {
                        // set the first with default serial to be the default (in case of using the same printer several times, for testing ...)
                        self.bambu_printer_model.index = self.bambu_printer_model.printers.len() - 1;
                        default_printer_set = true;
                    }
                    available_printers.push(bambu_printer_model.borrow().printer_selector_name.to_shared_string());

                    // notification from printer on events, should be treated for all printers,
                    // but selected printer should be considered as to what to update in the UI
                    if let Some(view_model_rc) = &self.view_model {
                        let trait_for_bambu_printer_rc: Rc<RefCell<dyn bambu::BambuPrinterObserver>> = view_model_rc.clone();
                        let trait_for_bambu_printer_weak: Weak<RefCell<dyn bambu::BambuPrinterObserver>> = Rc::downgrade(&trait_for_bambu_printer_rc);
                        bambu_printer_model.borrow_mut().subscribe(trait_for_bambu_printer_weak);
                    }
                    printer_index += 1; // index is increased only if printer is added to array
                }
                Err(e) => {
                    term_info!("[{}] Error initializing printer: {}", printer_number, e);
                }
            }
            printer_number += 1; // printer_number is always increased, even if printer is bad config
        }
        if !self.bambu_printer_model.printers.is_empty() {
            let default_printer = self.bambu_printer_model.printers[self.bambu_printer_model.index]
                .borrow()
                .printer_selector_name
                .to_shared_string();
            let available_printers = slint::ModelRc::new(slint::VecModel::from(available_printers));
            ui_app_state.invoke_set_printers_info(available_printers, default_printer.clone());
            ui_app_state.invoke_set_curr_printer(default_printer);
            self.register_printer_related_listeners();

            let moved_ui = self.ui_weak.clone();
            let moved_view_model = self.view_model.as_ref().unwrap().clone();
            // this select_printer handler CAN'T depend on printer because then it would need to change itself while running
            ui_app_backend.on_select_printer(move |selected_printer: SharedString| {
                // First stored UI for this printer for when we switch back to it
                Self::perform_select_printer(moved_ui.clone(), moved_view_model.clone(), &selected_printer);
            });
            self.framework
                .borrow()
                .spawner
                .spawn(printers_scheduled_store_state_task(
                    self.framework.clone(),
                    self.view_model.clone().unwrap(),
                ))
                .ok();

            self.framework
                .borrow()
                .spawner
                .spawn(store_printers_consume(self.view_model.clone().unwrap()))
                .ok();

            let trait_for_gcode_analyzer_rc: Rc<RefCell<dyn GcodeAnalyzerObserver>> = self.view_model.as_ref().unwrap().clone();
            let trait_for_gcode_analyzer_weak: Weak<RefCell<dyn GcodeAnalyzerObserver>> = Rc::downgrade(&trait_for_gcode_analyzer_rc);

            let task = Box::leak(Box::new(TaskStorage::new())).spawn(|| {
                fetch_gcode_analysis_task(
                    self.framework.clone(),
                    self.gcode_analysis_request_channel.clone(),
                    self.gcode_analysis_notification_channel.clone(),
                    trait_for_gcode_analyzer_weak,
                )
            });
            self.framework.borrow().spawner.spawn(task).ok();
        }

        // Initialize SpoolScale and weight related stuff

        let moved_view_model = self.view_model.as_ref().unwrap().clone();
        ui_app_backend.on_get_spools_core_list(move |filter| {
            let mut view_model_borrow = moved_view_model.borrow_mut();

            // separated to not borrow twice
            let user_cores_changed = view_model_borrow.app_config.borrow().user_cores_changed_by_web_config;
            let spools_cores_filter = &view_model_borrow.spools_cores_filter;

            if user_cores_changed || spools_cores_filter != filter.as_str() {
                view_model_borrow.regenerate_cores_weights_list(filter.as_str());
                view_model_borrow.spools_cores_filter = filter.to_string();
                view_model_borrow.app_config.borrow_mut().user_cores_changed_by_web_config = false;
            }
            view_model_borrow.cores_list_vec_rc.clone()
        });

        let moved_view_model = self.view_model.as_ref().unwrap().clone();
        ui_app_backend.on_get_spool_core_weight(move |id| *moved_view_model.borrow().spools_cores_weights.get(&id).unwrap_or(&0));

        let moved_view_model = self.view_model.as_ref().unwrap().clone();
        ui_app_backend.on_erase_previously_used_core_list(move || moved_view_model.borrow_mut().erase_previously_used_cores_list());

        let spools_core_filter = self.spools_cores_filter.clone();
        self.regenerate_cores_weights_list(&spools_core_filter);
    }

    pub fn regenerate_cores_weights_list(&mut self, filter: &str) {
        // Fill spool cores weights list

        self.spools_cores_weights.clear();

        let cores_list_vec = self
            .cores_list_vec_rc
            .as_any()
            .downcast_ref::<slint::VecModel<crate::app::SelectorOption>>()
            .unwrap();
        cores_list_vec.clear();

        let mut id = -1;
        let app_config_clone = self.app_config.clone();

        {
            // Add Clear/Unset/No-Core first

            id += 1;
            let selector_option = crate::app::SelectorOption {
                id,
                text: "Don't Encode Spool Core Weight".into(),
            };
            cores_list_vec.push(selector_option);
        }

        if let Some(user_cores) = &app_config_clone.borrow().user_cores {
            id = self.add_core_weights_csv_to_list(id, user_cores.as_str(), "My Spools List", filter);
        }
        if let Some(previously_used_cores) = &app_config_clone.borrow().previously_used_cores {
            id = self.add_core_weights_csv_to_list(id, previously_used_cores.as_str(), "Previously Used", filter);
        }
        let _id = self.add_core_weights_csv_to_list(id, SPOOLS_CATALOG, "SpoolEase Spools Catalog", filter);
    }

    pub fn add_to_previously_used_cores(&mut self, core_name: &str, core_weight: i32) {
        if core_name.is_empty() {
            return;
        }
        let line_start = format!("{core_name},"); // the ',' is important, because one name could include another
        let mut app_config_borrow = self.app_config.borrow_mut();
        let mut new_previously_used_cores;
        if let Some(user_cores) = &app_config_borrow.user_cores {
            let line_found = user_cores.lines().find(|line| line.starts_with(&line_start));
            if line_found.is_some() {
                return;
            }
        }
        if let Some(previously_used_cores) = &app_config_borrow.previously_used_cores {
            let line_found = previously_used_cores.lines().enumerate().find(|line| line.1.starts_with(&line_start));
            if let Some((index, line)) = line_found {
                if index == 0 {
                    return;
                } else {
                    let line_to_remove = format!("{line}\r\n");
                    new_previously_used_cores = previously_used_cores.replace(&line_to_remove, "");
                    new_previously_used_cores.insert_str(0, &format!("{core_name},{core_weight}\r\n"));
                }
            } else {
                new_previously_used_cores = format!("{core_name},{core_weight}\r\n{previously_used_cores}");
            }
        } else {
            new_previously_used_cores = format!("{core_name},{core_weight}\r\n");
        }

        // limit to 9 previously used
        if new_previously_used_cores.lines().count() > 9 {
            if let Some(last_crlf) = new_previously_used_cores.rfind("\r\n") {
                if let Some(last_2nd_crlf) = new_previously_used_cores[..last_crlf].rfind("\r\n") {
                    new_previously_used_cores = new_previously_used_cores[..last_2nd_crlf + 2].to_string();
                }
            }
        }

        let _ = app_config_borrow.set_previously_used_cores(Some(new_previously_used_cores));
        drop(app_config_borrow);
        let spools_cores_filter = self.spools_cores_filter.clone();
        self.regenerate_cores_weights_list(&spools_cores_filter);
    }
    pub fn erase_previously_used_cores_list(&mut self) {
        let _ = self.app_config.borrow_mut().set_previously_used_cores(None);
        let spools_cores_filter = self.spools_cores_filter.clone();
        self.regenerate_cores_weights_list(&spools_cores_filter);
    }

    pub fn add_core_weights_csv_to_list(&mut self, last_id: i32, csv: &str, title: &str, filter: &str) -> i32 {
        // returns last-id used

        let cores_list = self
            .cores_list_vec_rc
            .as_any()
            .downcast_ref::<slint::VecModel<crate::app::SelectorOption>>()
            .unwrap();
        let mut selector_option = crate::app::SelectorOption::default();
        let mut id = last_id;
        selector_option.id = -1;
        selector_option.text = title.into();
        cores_list.push(selector_option);

        csv.lines().for_each(|line| {
            let mut split = line.splitn(4, ',');
            while let (Some(desc), Some(weight)) = (split.next(), split.next()) {
                if !desc.is_empty() && !weight.is_empty() && (filter.is_empty() || desc.to_uppercase().contains(filter)) {
                    id += 1;
                    let selector_option = crate::app::SelectorOption {
                        id,
                        text: desc.trim().into(),
                    };
                    cores_list.push(selector_option);
                    if let Ok(weight) = weight.trim().parse() {
                        self.spools_cores_weights.insert(id, weight);
                    } else {
                        error!("Error in Spool Line: '{line}'");
                    }
                }
            }
        });
        id
    }

    fn perform_select_printer(
        moved_ui: slint::Weak<crate::app::AppWindow>,
        moved_view_model: Rc<RefCell<ViewModel>>,
        selected_printer: &SharedString,
    ) {
        // Collect printer view state to store until we switch back
        let current_shown_ams = moved_ui.unwrap().global::<crate::app::AppState>().get_curr_ams_id();
        let current_printer_selector_name = moved_ui.unwrap().global::<crate::app::AppState>().get_curr_printer();
        moved_view_model.borrow_mut().printers_view_state.insert(
            current_printer_selector_name.to_string(),
            PrinterUiState {
                curr_ams: Some(current_shown_ams),
            },
        );

        // Then process select
        let mut borrowed_view_model = moved_view_model.borrow_mut();
        let selected_printer_string = selected_printer.to_string();
        for (i, printer) in borrowed_view_model.bambu_printer_model.printers.iter().enumerate() {
            if selected_printer_string == printer.borrow().printer_selector_name {
                moved_ui
                    .unwrap()
                    .global::<crate::app::AppState>()
                    .invoke_set_curr_printer(selected_printer.to_shared_string());
                borrowed_view_model.bambu_printer_model.index = i;
                moved_ui.unwrap().global::<crate::app::AppState>().set_curr_ams_id(0); // while strange, this is importnat here for restoring curr_ams after, next call will set it to the first (in case 0 doesn't exist)
                borrowed_view_model.update_ui_from_printer(&borrowed_view_model.bambu_printer_model.printers[i].borrow());
                // now we'll resrore to the corret curr_ams if user was already there before, if not it will stay on the correct first ams
                if let Some(printer_view_state) = &borrowed_view_model.printers_view_state.get(&selected_printer_string) {
                    if let Some(past_curr_ams_id) = printer_view_state.curr_ams {
                        moved_ui.unwrap().global::<crate::app::AppState>().set_curr_ams_id(past_curr_ams_id);
                    }
                }
                borrowed_view_model.register_printer_related_listeners();
                break;
            }
        }
    }

    fn get_filament_info(&self, search_code: &str) -> Option<(bool, String, u32, u32)> {
        let app_config_borrow = self.app_config.borrow();
        let empty_list = String::new();
        let filament_lists = [BASE_FILAMENTS, app_config_borrow.custom_filaments.as_ref().unwrap_or(&empty_list)];

        let mut base = true;
        for filament_list in filament_lists {
            for line in filament_list.lines() {
                let mut split = line.split(',');
                if let (Some(code), Some(name), Some(nozzle_temp_low), Some(nozzle_temp_high)) =
                    (split.next(), split.next(), split.next(), split.next())
                {
                    if code == search_code {
                        let name = decode_csv_field(name);
                        let nozzle_temp_low = nozzle_temp_low.parse::<u32>().unwrap_or_default();
                        let nozzle_temp_high = nozzle_temp_high.parse::<u32>().unwrap_or_default();
                        return Some((base, name, nozzle_temp_low, nozzle_temp_high));
                    }
                }
            }
            base = false;
        }
        None
    }
    pub fn web_app_set_encode_info(&mut self, encode_info: &EncodeInfoDTO) {
        let ui = self.ui_weak.unwrap();

        // Initialize UI FrameworkState with framework information
        let ui_app_state = ui.global::<crate::app::AppState>();
        let mut encode_request = ui_app_state.get_curr_encode_request();
        encode_request.weight_advertised = encode_info.weight_advertised;
        if encode_request.weight_core != encode_info.weight_core {
            encode_request.color_name = "".to_shared_string()
        }
        encode_request.weight_core = encode_info.weight_core;
        encode_request.brand = encode_info.brand.to_shared_string();
        encode_request.filament_subtype = encode_info.filament_subtype.to_shared_string();
        encode_request.color_name = encode_info.color_name.to_shared_string();
        encode_request.note = encode_info.note.to_shared_string();
        if encode_request.tray_id == 998 || encode_request.tray_id == 999 {
            // Allow full editing only for staging and tray, enforced also on client so maybe redundant here
            let slicer_filament_enriched_info = if !encode_info.slicer_filament.is_empty() {
                self.get_filament_info(&encode_info.slicer_filament)
            } else {
                None
            };
            if let Some(tag_info) = self.encode_from_blank.as_mut() {
                if let Some(filament) = tag_info.filament.as_mut() {
                    filament.tray_type = encode_info.material.clone();
                    filament.tray_color = encode_info.color_code.clone();
                    filament.tray_info_idx = encode_info.slicer_filament.clone();
                    if let Some((_, _, nozzle_temp_low, nozzle_temp_high)) = slicer_filament_enriched_info {
                        filament.nozzle_temp_min = nozzle_temp_low;
                        filament.nozzle_temp_max = nozzle_temp_high;
                    }
                }
            }
        }

        ui_app_state.set_curr_encode_request(encode_request);
        self.calc_encode_request_display(crate::app::FilamentInfoMode::Encode);
    }

    pub fn web_app_get_encode_info(&self) -> EncodeInfoDTO {
        let ui = self.ui_weak.unwrap();

        // Initialize UI FrameworkState with framework information
        let ui_app_state = ui.global::<crate::app::AppState>();
        let encode_request = ui_app_state.get_curr_encode_request();
        if let Ok(tag_info) = self.tag_info_to_encode(&encode_request) {
            EncodeInfoDTO {
                tray_id: encode_request.tray_id,
                brand: tag_info.brand.unwrap_or_default(),
                color_name: tag_info.color_name.unwrap_or_default(),
                filament_subtype: tag_info.filament_subtype.unwrap_or_default(),
                note: tag_info.note.unwrap_or_default(),
                id: tag_info.id.unwrap_or_default(),
                weight_advertised: tag_info.weight_advertised.unwrap_or_default(),
                weight_core: tag_info.weight_core.unwrap_or_default(),
                tag_id: hex::encode_upper(tag_info.tag_id.unwrap_or_default()),
                color_code: tag_info.filament.as_ref().unwrap_or(&FilamentInfo::default()).tray_color.clone(),
                material: tag_info.filament.as_ref().unwrap_or(&FilamentInfo::default()).tray_type.clone(),
                slicer_filament: tag_info.filament.unwrap_or_default().tray_info_idx,
            }
        } else {
            EncodeInfoDTO {
                tray_id: -1,
                ..Default::default()
            }
        }
    }

    fn set_staging_to_tray_direct(
        &mut self,
        filament_staging: &Rc<RefCell<FilamentStaging>>,
        bambu_printer: &mut BambuPrinter,
        ui: &slint::Weak<crate::app::AppWindow>,
        tray_id: i32,
    ) {
        let mut filament_staging = filament_staging.borrow_mut();
        if let Some(tag_info) = filament_staging.tag_info() {
            bambu_printer.set_tray_filament(tray_id, tag_info);
            filament_staging.clear();
            ui.unwrap().global::<crate::app::AppState>().invoke_empty_spool_staging();
            let (ams_id, tray_id) = BambuPrinter::get_ams_and_tray_id(tray_id as usize);
            let ams_id = ams_id as i32;
            let tray_id = tray_id as i32;
            ui.unwrap().global::<crate::app::AppState>().invoke_tray_update_succeeded(
                bambu_printer.printer_selector_name.to_shared_string(),
                ams_id,
                tray_id,
            );
        }
    }

    fn set_staging_to_tray(
        filament_staging: &Rc<RefCell<FilamentStaging>>,
        bambu_printer: &Rc<RefCell<BambuPrinter>>,
        ui: &slint::Weak<crate::app::AppWindow>,
        tray_id: i32,
    ) {
        let mut filament_staging = filament_staging.borrow_mut();
        if let Some(tag_info) = filament_staging.tag_info() {
            bambu_printer.borrow_mut().set_tray_filament(tray_id, tag_info);
            filament_staging.clear();
            ui.unwrap().global::<crate::app::AppState>().invoke_empty_spool_staging();
            let (ams_id, tray_id) = BambuPrinter::get_ams_and_tray_id(tray_id as usize);
            let ams_id = ams_id as i32;
            let tray_id = tray_id as i32;

            let selected_in_ui = ui.unwrap().global::<crate::app::AppState>().get_curr_printer();
            warn!(
                "UI Selected Printer: [{}], setting tray of printer: [{}]",
                selected_in_ui,
                bambu_printer.borrow().printer_selector_name
            );

            ui.unwrap().global::<crate::app::AppState>().invoke_tray_update_succeeded(
                bambu_printer.borrow().printer_selector_name.to_shared_string(),
                ams_id,
                tray_id,
            );
        }
    }

    pub fn tag_info_to_encode(&self, encode_request: &EncodeRequest) -> Result<TagInformation, String> {
        let moved_bambu_printer = self.bambu_printer_model.clone();
        let tray_id = if let Ok(tray_id) = usize::try_from(encode_request.tray_id) {
            tray_id
        } else {
            return Err("Currently Not Encoding".to_string());
        };
        let mut tag_info_to_encode = if tray_id == 998 || tray_id == 999 {
            // Encode from blank, manual data only
            let tag_info = self.encode_from_blank.clone();
            tag_info.unwrap() // when we get here this should always pass
        } else {
            match moved_bambu_printer.borrow().get_tag_info_to_encode(tray_id) {
                Ok(tag_info) => tag_info,
                Err(err) => {
                    // hopefully no borrowing issues since calling into ui in a callback
                    return Err(err); // signals an error, UI will not continue
                }
            }
        };
        // let spool_scale_weight = moved_spool_scale.borrow().weight;
        tag_info_to_encode.id = if !encode_request.id.is_empty() {
            Some(encode_request.id.clone().into())
        } else {
            None
        };
        tag_info_to_encode.weight_new = (encode_request.weight_new != 0).then_some(encode_request.weight_new);
        tag_info_to_encode.weight_advertised = (encode_request.weight_advertised != 0).then_some(encode_request.weight_advertised);
        tag_info_to_encode.weight_core = (encode_request.weight_core != 0).then_some(encode_request.weight_core);
        tag_info_to_encode.brand = (!encode_request.brand.trim().is_empty()).then(|| encode_request.brand.trim().to_string());
        tag_info_to_encode.filament_subtype =
            (!encode_request.filament_subtype.trim().is_empty()).then(|| encode_request.filament_subtype.trim().to_string());
        tag_info_to_encode.color_name = (!encode_request.color_name.trim().is_empty()).then(|| encode_request.color_name.trim().to_string());
        tag_info_to_encode.note = (!encode_request.note.trim().is_empty()).then(|| encode_request.note.trim().to_string());
        Ok(tag_info_to_encode)
    }

    fn register_printer_related_listeners(&mut self) {
        // handler for request from UI to move to staging, need to work only on selected printer
        let moved_filament_staging = self.filament_staging.clone();
        let moved_bambu_printer = self.bambu_printer_model.clone();
        let moved_ui = self.ui_weak.clone();
        self.ui_weak
            .unwrap()
            .global::<crate::app::AppBackend>()
            .on_set_staging_to_tray(move |tray_id: i32| {
                Self::set_staging_to_tray(&moved_filament_staging, &moved_bambu_printer, &moved_ui, tray_id);
            });

        // handler for request from UI to encode a spool, need to work only on selected printer
        let moved_bambu_printer = self.bambu_printer_model.clone();
        let moved_spool_tag = self.spool_tag_model.clone();
        let moved_ui = self.ui_weak.clone();
        let moved_view_model = self.view_model.clone().unwrap();
        let moved_spool_scale = self.spool_scale_model.clone();
        moved_ui.unwrap().global::<crate::app::AppBackend>().on_encode_tag(move |encode_request| {
            info!("Request to encode tag with tray {} info", encode_request.tray_id);
            // Start with adding the core info to the previoysly used list
            if !encode_request.core_name.is_empty() {
                moved_view_model
                    .borrow_mut()
                    .add_to_previously_used_cores(encode_request.core_name.as_str(), encode_request.weight_core);
            }

            let tray_id = usize::try_from(encode_request.tray_id).unwrap();
            // Fill in tag information
            let mut tag_info_to_encode = match moved_view_model.borrow().tag_info_to_encode(&encode_request) {
                Ok(tag_info) => tag_info,
                Err(err) => {
                    moved_ui
                        .unwrap()
                        .global::<crate::app::AppState>()
                        .invoke_encoding_failed(err.to_shared_string());
                    return 0;
                }
            };

            tag_info_to_encode.encode_time = store_safe_time_now();

            // In case of encode from blank or staging (which is copied to blank), clean the scratch-pad used
            // If want to allow to return in case of cancel, need to move this to after encode success
            if tray_id == 998 || tray_id == 999 {
                moved_view_model.borrow_mut().encode_from_blank = None;
            }

            // Next encode
            let bambu_printer_borrow = moved_bambu_printer.borrow();
            let descriptor_res = if tray_id == 999 || tray_id == 998 {
                &tag_info_to_encode.to_descriptor(None, None)
            } else {
                &tag_info_to_encode.to_descriptor(
                    Some(&bambu_printer_borrow.printer_name),
                    Some(&bambu_printer_borrow.printer_uuid_to_encode),
                )
            };
            let spool_tag = moved_spool_tag.borrow();
            if let Some(descriptor) = descriptor_res {
                let encode_cookie = EncodeCookie {
                    scale_weight: moved_spool_scale.borrow().weight,
                    spool_id: encode_request.id.into(),
                };
                let cookie = serde_json::to_string(&encode_cookie).unwrap_or_default();
                spool_tag.write_tag(descriptor, tray_id, cookie);
            }
            info!("Sent the write request of tray {}", tray_id);
            // TODO: Get proper timeout fron config and pass it in the write_tag to spool_tag
            15
        });

        // // handler for request from UI to reset printer, should work only on selected printer
        // let moved_bambu_printer = self.bambu_printer_model.clone();
        // let moved_ui = self.ui_weak.clone();
        // self.ui_weak.unwrap().global::<crate::app::AppBackend>().on_reset_printer(move || {
        //     moved_bambu_printer.borrow_mut().reset_printer();
        //     moved_ui.unwrap().global::<crate::app::AppState>().invoke_reset_printer();
        // });

        // handle encoding related listener(s) - this depends on current printer

        let moved_ui = self.ui_weak.clone();
        let moved_store = self.store.clone();
        let moved_view_model = self.view_model.as_ref().unwrap().clone();
        self.ui_weak
            .unwrap()
            .global::<crate::app::AppBackend>()
            .on_fill_encode_request_from_spool_id(move |spool_id| {
                let moved_ui = moved_ui.unwrap();
                let ui_app_state = moved_ui.global::<crate::app::AppState>();
                let mut encode_request = ui_app_state.get_curr_encode_request();
                let encode_request_display = ui_app_state.get_curr_encode_request_display();
                if let Some(spool_rec) = moved_store.get_spool_by_id(spool_id.as_str()) {
                    let from_blank_request = encode_request.tray_index == 998 || encode_request.tray_index == 999;
                    if spool_rec.tag_id.is_empty() {
                        if spool_rec.material_type.as_str() == encode_request_display.filament_type.as_str() || from_blank_request {
                            encode_request.id = spool_id;
                            if let Some(weight_advertised) = spool_rec.weight_advertised {
                                encode_request.weight_advertised = weight_advertised;
                            }
                            if let Some(weight_core) = spool_rec.weight_core {
                                encode_request.weight_core = weight_core;
                            }
                            encode_request.brand = spool_rec.brand.to_shared_string();
                            encode_request.filament_subtype = spool_rec.material_subtype.to_shared_string();
                            encode_request.color_name = spool_rec.color_name.to_shared_string();
                            encode_request.note = spool_rec.note.to_shared_string();

                            if encode_request.tray_index == 998 || encode_request.tray_index == 999{
                                let mut view_model_borrow_mut = moved_view_model.borrow_mut();
                                if from_blank_request {
                                    let slicer_filament_enriched_info = if !spool_rec.slicer_filament.is_empty() {
                                        // if povided slicer filament setting, then set temps accordingly
                                        view_model_borrow_mut.get_filament_info(&spool_rec.slicer_filament)
                                    } else { None };

                                    let filament_info = view_model_borrow_mut.encode_from_blank.as_mut().unwrap().filament.as_mut().unwrap();

                                    filament_info.tray_type = spool_rec.material_type;
                                    filament_info.tray_color = spool_rec.color_code;
                                    filament_info.tray_info_idx = spool_rec.slicer_filament;
                                    if !filament_info.tray_info_idx.is_empty() {
                                        if let Some((_, _, nozzle_temp_low, nozzle_temp_high)) = slicer_filament_enriched_info {
                                            filament_info.nozzle_temp_min = nozzle_temp_low;
                                            filament_info.nozzle_temp_max = nozzle_temp_high;
                                        } else {
                                            error!("Tray_info_idx supplied from inventory without information (something probably changed from encoding to these days in custom filaments config)");
                                            filament_info.nozzle_temp_min = 0; // Real values will be used when based on tray_type (material) when needed
                                            filament_info.nozzle_temp_max = 0; // Real values will be used when based on tray_type (material) when needed
                                        }
                                    } else {
                                        filament_info.nozzle_temp_min = 0; // Real values will be used when based on tray_type (material) when needed
                                        filament_info.nozzle_temp_max = 0; // Real values will be used when based on tray_type (material) when needed
                                    }
                                }
                            }
                            ui_app_state.set_curr_encode_request(encode_request);
                            "".to_shared_string()
                        } else {
                            "Material Mismatch".to_shared_string()
                        }
                    } else {
                        "Already Tagged".to_shared_string()
                    }
                } else {
                    "ID Not Found".to_shared_string()
                }
            });

        let moved_view_model = self.view_model.as_ref().unwrap().clone();
        self.ui_weak.unwrap().global::<crate::app::AppBackend>().on_notify_post_encode(move || {
            moved_view_model.borrow_mut().encode_from_blank = None;
        });

        let moved_view_model = self.view_model.as_ref().unwrap().clone();
        self.ui_weak
            .unwrap()
            .global::<crate::app::AppBackend>()
            .on_notify_start_encode(move |tray_id| {
                if tray_id == 999 {
                    let staging_tag_info = moved_view_model.borrow().filament_staging.borrow().tag_info().clone();
                    if staging_tag_info.is_some() {
                        moved_view_model.borrow_mut().encode_from_blank = staging_tag_info;
                        return;
                    }
                }
                if tray_id == 998 || tray_id == 999 {
                    // 999 if previous if didn't work out
                    let blank_tag_info = TagInformation {
                        filament: Some(FilamentInfo::new()),
                        ..Default::default()
                    };
                    moved_view_model.borrow_mut().encode_from_blank = Some(blank_tag_info);
                }
            });
        let moved_view_model = self.view_model.as_ref().unwrap().clone();
        self.ui_weak
            .unwrap()
            .global::<crate::app::AppBackend>()
            .on_calc_encode_request_display(move |mode| {
                moved_view_model.borrow().calc_encode_request_display(mode);
            });
    }

    fn calc_encode_request_display(&self, mode: crate::app::FilamentInfoMode) {
        let moved_ui = self.ui_weak.unwrap();
        let ui_app_state = moved_ui.global::<crate::app::AppState>();
        let mut encode_request = ui_app_state.get_curr_encode_request();
        let mut encode_request_display = ui_app_state.get_curr_encode_request_display();
        encode_request_display.pa_line1 = "".into();
        encode_request_display.pa_line2 = "".into();
        let tray_id = encode_request.tray_id;

        let staging_borrow = self.filament_staging.borrow();
        let bambu_borrow = self.bambu_printer_model.borrow();

        // This is a bit delicate here
        // To display/encode there are two potential sources of information - the tray information and the tag information.
        // tray information exists with tray_is is external tray or ams trays
        // tag information exist when a tray has a tag inside (not always) or in staging
        // when from blank (998) nothing is available at start but built in encode_from_blank variable
        // Then, tray takes precedence over tag information
        // Tag information is used based on if there are differences in tray information and tag information (so for example color name will be used if color codes are the same)
        // On top of that, there are differences in case of encode and in case of display
        //  In case of display we show the inventory-id, but in case of encode we don't want to override the inventory-id so it won't show the inventory-id and will be empty
        //  Later either the user can set an explicit ID to use, or on the tag_id itself when encoded if match will mark off the previous id record and create a new one,

        // Start first with getting the relevant tag information and the filament information
        let (filament_info, tag_info) = match tray_id {
            -1 => {
                // Special case to avoid a call after encode_request is cleared
                return;
            }
            999 | 998 => {
                // For staging (999) for View - use the staging data, for encode use blank data (which is copied into when encode starts)
                let tag_info_source = if tray_id == 999 && mode == FilamentInfoMode::View {
                    staging_borrow.tag_info()
                } else {
                    &self.encode_from_blank
                };
                if let Some(tag_info) = tag_info_source {
                    if let Some(filament_info) = &tag_info.filament {
                        if let Some((nozzle_diameter, calibration)) = &tag_info.calibrations.iter().next() {
                            encode_request_display.pa_line2 = format!("{}, {}", calibration.k_value, calibration.name).into();
                            encode_request_display.pa_line1 = format!("{}, {}", tag_info.calibrations_printer_name, nozzle_diameter).into();
                        }
                        (Some(filament_info.clone()), staging_borrow.tag_info())
                    } else {
                        (None, &None)
                    }
                } else {
                    (None, &None)
                }
            }
            254 => {
                // External Tray
                let tray = &bambu_borrow.virt_tray();
                if let Some(calibration) = bambu_borrow.get_tray_calibration(tray) {
                    encode_request_display.pa_line2 = format!("{}, {}", calibration.k_value, calibration.name,).into();
                }
                if let bambu::Filament::Known(filament_info) = &tray.filament {
                    (Some(filament_info.clone()), &tray.meta_info.tag_info)
                } else {
                    (None, &None)
                }
            }
            0..15 => {
                // Standard trays
                // let bambu = moved_bambu.borrow();
                let tray = &bambu_borrow.ams_trays()[tray_id as usize];
                // if let Some(calibration) = bambu_borrow.get_tray_calibration(tray) {
                //     encode_request_display.pa_line2 = format!("{}, {}", calibration.k_value, calibration.name,).into();
                // }
                if let bambu::Filament::Known(filament_info) = &tray.filament {
                    (Some(filament_info.clone()), &tray.meta_info.tag_info)
                } else {
                    (None, &None)
                }
            }
            _ => {
                error!("UI request to update display for tray out of range, software error or printer issue");
                (None, &None)
            }
        };

        if let Some(tag_info) = tag_info {
            if let Some(id) = &tag_info.id {
                encode_request.id = id.to_shared_string();
            }
        }

        // Case when id was selected (differently) from UI
        // In such case need to fetch from store the data and fill it in

        let first_request_to_display = encode_request_display.filament_type.is_empty();

        if let Some(filament_info) = filament_info {
            if first_request_to_display {
                // checking tray_type is empty to know if it is the first time, later if user changes to empty some value it shouldn't be overriden
                if let Some(tag_info) = tag_info {
                    let mut material_changed_from_tag = false;
                    let mut color_code_changed_from_tag = false;
                    if let Some(tag_filament_info) = &tag_info.filament {
                        if tag_filament_info.tray_color != filament_info.tray_color {
                            color_code_changed_from_tag = true;
                        }
                        if tag_filament_info.tray_type != filament_info.tray_type || tag_filament_info.tray_info_idx != filament_info.tray_info_idx {
                            material_changed_from_tag = true;
                        }
                    }
                    // if tray filament type didn't change from the tag, then we can use the brand, subtype,
                    if !material_changed_from_tag {
                        if encode_request.brand.is_empty() && tag_info.brand.is_some() {
                            encode_request.brand = tag_info.brand.as_ref().unwrap().to_shared_string();
                        }
                        if encode_request.filament_subtype.is_empty() && tag_info.filament_subtype.is_some() {
                            encode_request.filament_subtype = tag_info.filament_subtype.as_ref().unwrap().to_shared_string();
                        }
                    }

                    // if tray filament color code didn't change from tag we can use tag color name, otherwise we calculate name again
                    #[allow(clippy::collapsible_if)]
                    if !color_code_changed_from_tag {
                        if encode_request.color_name.is_empty() && tag_info.color_name.is_some() {
                            encode_request.color_name = tag_info.color_name.as_ref().unwrap().to_shared_string();
                        }
                    }
                    // If there is tag_info,  there is always also filament_info so can do it here instead of a separate tag_info unwrapping outside the if filament_info
                    if encode_request.note.is_empty() && tag_info.note.is_some() {
                        encode_request.note = tag_info.note.as_ref().unwrap().to_shared_string();
                    }
                    if encode_request.weight_advertised == 0 && tag_info.weight_advertised.is_some() {
                        encode_request.weight_advertised = tag_info.weight_advertised.unwrap();
                    }
                    if encode_request.weight_core == 0 && tag_info.weight_core.is_some() {
                        encode_request.weight_core = tag_info.weight_core.unwrap();
                    }
                    // in case of encode we don't copy weight_new
                    #[allow(clippy::collapsible_if)]
                    if mode == FilamentInfoMode::View {
                        if encode_request.weight_new == 0 && tag_info.weight_new.is_some() {
                            encode_request.weight_new = tag_info.weight_new.unwrap();
                        }
                    }
                }

                if encode_request.color_name.is_empty() && filament_info.tray_color.len() >= 6 {
                    let color = u32::from_str_radix(&filament_info.tray_color[..6], 16).unwrap() + 0xFF000000; // the plus 0xFF at the end is fo add alpha
                    let color = slint::Color::from_argb_encoded(color);
                    let color_name_info = get_color_name(color.red(), color.green(), color.blue());
                    encode_request.color_name = color_name_info.0.to_shared_string();
                    if mode == crate::app::FilamentInfoMode::View {
                        encode_request.color_name = format!("({})", encode_request.color_name).to_shared_string();
                    }
                }
            }

            encode_request_display.color_code = filament_info.tray_color[..filament_info.tray_color.len().min(8)].into();
            if filament_info.tray_color.len() >= 6 {
                let color = u32::from_str_radix(&filament_info.tray_color[..6], 16).unwrap() + 0xFF000000; // the plus 0xFF at the end is fo add alpha
                encode_request_display.title_color = slint::Color::from_argb_encoded(color);
            }
            encode_request_display.temp_min = filament_info.nozzle_temp_min.try_into().unwrap_or_default();
            encode_request_display.temp_max = filament_info.nozzle_temp_max.try_into().unwrap_or_default();
            encode_request_display.filament_type = filament_info.tray_type.into();
            // TODO: Add support for custom filaments here

            if let Some((base, slicer_name, _, _)) = self.get_filament_info(&filament_info.tray_info_idx) {
                encode_request_display.slicer_name = format!("{slicer_name} ({})", if base { "Base" } else { "Custom" }).into();
            } else {
                encode_request_display.slicer_name = "Unknown Filament".into();
            }

            if encode_request_display.pa_line1.is_empty() && !encode_request_display.pa_line2.is_empty() {
                // if there is a line 2 but line 1 was not filled (staging case)
                encode_request_display.pa_line1 = format!(
                    "{}, {}",
                    bambu_borrow.printer_name,
                    bambu_borrow.nozzle_diameter().as_ref().unwrap_or(&"Unknown".to_string())
                )
                .into();
            }

            if first_request_to_display && encode_request.brand.is_empty() {
                if let Some(brand_name) = get_brand_from_text(encode_request_display.pa_line2.as_str()) {
                    encode_request.brand = brand_name.to_shared_string();
                } else if let Some(brand_name) = get_brand_from_text(encode_request_display.slicer_name.as_str()) {
                    encode_request.brand = brand_name.to_shared_string();
                }
                if !encode_request.brand.is_empty() && mode == crate::app::FilamentInfoMode::View {
                    encode_request.brand = format!("({})", encode_request.brand).to_shared_string();
                }
            }

            ui_app_state.set_curr_encode_request_display(encode_request_display);
            if first_request_to_display {
                ui_app_state.set_curr_encode_request(encode_request);
            }
        }
    }

    fn tag_info_to_ui_spool_info_direct(&self, bambu_printer_borrow: &BambuPrinter, tag_info: &TagInformation) -> Option<crate::app::UiSpoolInfo> {
        tag_info.filament.as_ref()?; // returns None if tag_info.filament is None
        let filament_info = tag_info.filament.as_ref().unwrap();

        let color = u32::from_str_radix(&filament_info.tray_color[..6], 16).unwrap() + 0xFF000000; // the plus 0xFF at the end is fo add alpha

        let mut final_k = bambu_printer_borrow.get_tag_k_for_current_nozzle(tag_info);
        if let Some(calibration) = tag_info
            .calibrations
            .get(bambu_printer_borrow.nozzle_diameter().as_ref().unwrap_or(&"NA".to_string()))
        {
            let source_k = &calibration.k_value;
            if source_k != &final_k {
                final_k = format!("?{final_k}");
            }
        }
        let ui_spool_info = crate::app::UiSpoolInfo {
            id: tag_info.id.clone().unwrap_or_default().to_shared_string(),
            color: slint::Color::from_argb_encoded(color),
            k: SharedString::from(final_k),
            material: filament_info.tray_type.to_shared_string(),
            weight_core: tag_info.weight_core.unwrap_or_default(),
        };
        Some(ui_spool_info)
    }

    fn tag_info_to_ui_spool_info(&self, tag_info: &TagInformation) -> Option<crate::app::UiSpoolInfo> {
        let bambu_printer_borrow = self.bambu_printer_model.borrow();
        self.tag_info_to_ui_spool_info_direct(&bambu_printer_borrow, tag_info)
    }

    fn update_ui_from_printer(&self, bambu_printer: &BambuPrinter) {
        // note - accepting bambu_printer rather than taking from self, because it may be called during callback on_trays_update,
        // and that's taking place when it's already borrowed and another borrow will panic

        let ui = self.ui_weak.unwrap();

        // ----- handle number of ams's and curr_ams -----
        if let Some(mut ams_exist_bits) = bambu_printer.ams_exist_bits {
            let mut ams_exist_vec = Vec::<i32>::new();
            let mut first_ams = -1;
            for ams_id in 0..=3 {
                if ams_exist_bits & 1 != 0 {
                    ams_exist_vec.push(ams_id);
                    if first_ams == -1 {
                        first_ams = ams_id;
                    }
                }
                ams_exist_bits >>= 1;
            }
            let ams_exists: Rc<slint::VecModel<i32>> = Rc::new(slint::VecModel::from(ams_exist_vec));
            let ams_exists = slint::ModelRc::from(ams_exists);
            ui.global::<crate::app::AppState>().set_ams_exists(ams_exists);
            let current_shown_ams = ui.global::<crate::app::AppState>().get_curr_ams_id();
            if first_ams > current_shown_ams {
                ui.global::<crate::app::AppState>().set_curr_ams_id(first_ams);
            }
        }

        // ----- handle trays view update ----
        let trays_state_rc = ui.global::<crate::app::AppState>().get_trays_state();
        // let trays_state_rc = ui.get_trays_state();
        let trays_state = trays_state_rc;
        for tray_row in 0..trays_state.row_count() {
            let tray_id = trays_state.row_data(tray_row).unwrap().id;
            let curr_tray = if tray_id == 254 {
                bambu_printer.virt_tray()
            } else {
                &bambu_printer.ams_trays()[tray_id as usize]
            };
            let mut ui_tray = trays_state.row_data(tray_row).unwrap().clone();
            ui_tray.spool_state = crate::app::UiTrayState::from(&curr_tray.state);
            if let bambu::Filament::Known(filament_info) = &curr_tray.filament {
                let color = u32::from_str_radix(&filament_info.tray_color[..6], 16).unwrap() + 0xFF000000; // the plus at the end is fo add alpha
                ui_tray.filament.color = slint::Color::from_argb_encoded(color);
                ui_tray.filament.material = slint::SharedString::from(&filament_info.tray_type);
                ui_tray.filament.state = crate::app::UiFilamentState::Known;
            } else {
                ui_tray.filament.state = crate::app::UiFilamentState::Unknown;
            }
            ui_tray.tagged = curr_tray.meta_info.tag_info.is_some();
            // let k_value_unformatted = curr_tray.k.as_ref().unwrap_or(&"(0.020)".to_string()).clone();
            let k_value_unformatted = bambu_printer.get_tray_resolved_k_value(curr_tray);
            // let k_value_for_ui = k_value_for_ui(&k_value_unformatted);
            ui_tray.k = SharedString::from(k_value_unformatted);
            ui_tray.weight_display = self.weight_display(curr_tray);
            trays_state.set_row_data(tray_row, ui_tray);
        }
    }

    fn weight_display(&self, tray: &Tray) -> SharedString {
        let mut res = SharedString::new();
        if tray.meta_info.consumed_since_load != 0.0 {
            res = slint::format!("-{:.1}g", tray.meta_info.consumed_since_load);
        }
        if let Some(tag_info) = &tray.meta_info.tag_info {
            if let Some(id) = &tag_info.id {
                if let Some(spool) = self.store.get_spool_by_id(id) {
                    if let (Some(weight_core), Some(weight_current)) = (spool.weight_core, spool.weight_current) {
                        let realtime_weight = (weight_current - weight_core) as f32 - tray.meta_info.consumed_since_load;
                        res = slint::format!("{:.1}g", realtime_weight);
                    } else if let (Some(weight_current), Some(weight_new), Some(weight_advertised)) =
                        (spool.weight_current, spool.weight_new, spool.weight_advertised)
                    {
                        let realtime_weight = (weight_current - (weight_new - weight_advertised)) as f32 - tray.meta_info.consumed_since_load;
                        res = slint::format!("{:.1}g", realtime_weight);
                    }

                    // weight_left:
                    //   weight_current && weight_core
                    //     ? weight_current - weight_core
                    //     : weight_current && weight_advertised && weight_new
                    //       ? weight_current - (weight_new - weight_advertised)
                    //       : null,
                }
            }
        }
        res
    }
}

impl From<&TrayState> for crate::app::UiTrayState {
    fn from(v: &TrayState) -> crate::app::UiTrayState {
        match v {
            TrayState::Unknown => crate::app::UiTrayState::Unknown,
            TrayState::Empty => crate::app::UiTrayState::Empty,
            TrayState::Spool => crate::app::UiTrayState::Spool,
            TrayState::Reading => crate::app::UiTrayState::Reading,
            TrayState::Ready => crate::app::UiTrayState::Ready,
            TrayState::Loading => crate::app::UiTrayState::Loading,
            TrayState::Unloading => crate::app::UiTrayState::Unloading,
            TrayState::Loaded => crate::app::UiTrayState::Loaded,
        }
    }
}

impl BambuPrinterObserver for ViewModel {
    fn on_trays_update(
        &mut self,
        bambu_printer: &mut BambuPrinter,
        prev_trays_bits: &TrayBits,
        new_trays_bits: &TrayBits,
        removed_tags: &HashMap<usize, TagInformation>,
    ) {
        // note - accepting bambu_printer rather than taking from self, because it's already borrowed and another borrow will panic
        let current_selected_printer = self.bambu_printer_model.index;

        if bambu_printer.printer_index == current_selected_printer {
            self.update_ui_from_printer(bambu_printer);
        }

        // If staging is loaded from scanned/encoded then check spool load cases
        if ![StagingOrigin::Unloaded, StagingOrigin::Empty].contains(self.filament_staging.borrow().origin()) {
            // first - if there is a spool (from scan/encode) in staging, and a spool is loaded then
            // at the moment of loading notify ui so it can reset the staging timer in case it is too low
            // and won't reach read_done before timer is out
            if let Some(new_trays_monitored_bits) = new_trays_bits.tray_exist_bits {
                let prev_trays_monitored_bits = prev_trays_bits.tray_exist_bits.unwrap_or(0);
                let mut trays_monitored_loaded = Vec::new();
                for tray_id in 0..bambu_printer.ams_trays().len() {
                    let prev_tray_monitored_bit = ((prev_trays_monitored_bits >> tray_id) & 0x01) != 0;
                    let new_tray_monitored_bit = ((new_trays_monitored_bits >> tray_id) & 0x01) != 0;
                    if !prev_tray_monitored_bit && new_tray_monitored_bit {
                        trays_monitored_loaded.push(tray_id);
                    }
                }
                // if bambu_printer.printer_number == 1 { // UNREMARK FOR TESTS WITH ONE PRINTER
                if trays_monitored_loaded.len() == 1 {
                    self.ui_weak
                        .unwrap()
                        .global::<crate::app::AppState>()
                        .invoke_spool_loaded_when_staging_loaded();
                }
            }

            // ----- Handle loading when there is something in staging -----
            // If the staging is loaded and only a SINGLE slot SWITCHED to reading update it to the stating filament info
            // trace!("------------------------------------------------------");
            // trace!(">>>>> prev : {prev_trays_bits:?}\n >>>>> next: {new_trays_bits:?}");
            // trace!("------------------------------------------------------");
            if let Some(new_trays_monitored_bits) = new_trays_bits.tray_read_done_bits {
                let prev_trays_monitored_bits = prev_trays_bits.tray_read_done_bits.unwrap_or(0);
                let mut trays_monitored_loaded = Vec::new();
                for tray_id in 0..bambu_printer.ams_trays().len() {
                    let prev_tray_monitored_bit = ((prev_trays_monitored_bits >> tray_id) & 0x01) != 0;
                    let new_tray_monitored_bit = ((new_trays_monitored_bits >> tray_id) & 0x01) != 0;
                    if !prev_tray_monitored_bit && new_tray_monitored_bit {
                        trays_monitored_loaded.push(tray_id);
                    }
                }
                // if bambu_printer.printer_number == 1 { // UNREMARK FOR TESTS WITH ONE PRINTER
                if trays_monitored_loaded.len() == 1 {
                    let only_monitored_tray = trays_monitored_loaded[0];
                    info!("Single tray {only_monitored_tray} is loading now");
                    self.set_staging_to_tray_direct(
                        &self.filament_staging.clone(),
                        bambu_printer,
                        &self.ui_weak.clone(),
                        only_monitored_tray as i32,
                    );
                }
                // }
            }
        }

        // Unloaded spool case - load tag if exist on that spool to staging (for weighting)
        if let Some(removed_tag) = removed_tags.iter().next() {
            let mut filament_staging = self.filament_staging.borrow_mut();
            if [StagingOrigin::Empty, StagingOrigin::Unloaded].contains(filament_staging.origin()) {
                // only if empty or was unloaded (so not scanned or encoded)
                if let Some(ui_spool_info) = self.tag_info_to_ui_spool_info_direct(bambu_printer, removed_tag.1) {
                    filament_staging.set_tag_info(removed_tag.1.clone(), filament_staging::StagingOrigin::Unloaded);
                    self.ui_weak
                        .unwrap()
                        .global::<crate::app::AppState>()
                        .invoke_update_spool_staging(ui_spool_info, crate::app::SpoolStagingState::Unloaded);
                }
            }
        }
    }

    fn on_printer_connect_status(&self, bambu_printer: &mut BambuPrinter, status: bool) {
        if status {
            // TODO: I can't borrow at this stage because my_mqtt reports this and need to borrow_mut so now can't borrow.
            //       Need to switch to the notifications coming from a notifier object and not directly from the objects.
            //       Or switch to a message loop notifications (which is a major change to the code, but more correct for these types of apps)
            //       So here I know it arrives here only if boot is successful, but in other applications this might not be enough
            // if self.app_config.borrow().boot_completed() {
            term_info!(&"-".repeat(67));
            term_info!("Printer [{}] connected successfully", bambu_printer.printer_number);
            term_info!(&"-".repeat(67));
            self.ui_weak
                .unwrap()
                .global::<crate::app::AppState>()
                .invoke_printer_connected(bambu_printer.printer_selector_name.to_shared_string());
        } else {
            term_info!("[{}] Printer disconnected", bambu_printer.printer_number);
        }
    }

    fn on_request_gcode_analysis(&mut self, printer: &mut BambuPrinter, print_project: &PrintProject) -> i32 {
        let ip = printer.printer_ip;
        let serial = printer.printer_serial.clone();
        let access_code = printer.printer_access_code.clone();
        let printer_number = printer.printer_number;
        let printer_index = printer.printer_index;
        self.gcode_last_job_number += 1;

        let subtask_name = print_project.subtask_name.clone();
        let plate_idx = print_project.plate_idx;
        let threemf_url = print_project.threemf_url.clone();
        let gcode_filename_in_3mf = print_project.gcode_filename_in_3mf.clone();

        info!("[{printer_number}] Received request for gcode analysis {subtask_name}, plate {plate_idx}");

        let gcode_analysis_request = GcodeAnalysisRequest {
            fetch_3mf: printer.fetch_3mf,
            ip,
            serial,
            access_code,
            printer_number,
            printer_index,
            subtask_name,
            plate_idx,
            job_number: self.gcode_last_job_number,
            threemf_url,
            gcode_filename_in_3mf,
        };

        // scale
        // match self.spool_scale_model.borrow_mut().request_gcode_analysis(gcode_analysis_request) {
        //     Ok(_) => {
        //         self.gcode_last_job_number
        //     }
        //     Err(err) => {
        //         error!("{err}");
        //         0
        //     }
        // }

        match self.gcode_analysis_request_channel.try_send(gcode_analysis_request) {
            Ok(_) => self.gcode_last_job_number,
            Err(err) => {
                error!("Failed sending request for gcode analysis within console : {err:?}");
                0
            }
        }
    }

    fn on_cancel_gcode_analysis(&mut self, job_number: i32) {
        self.gcode_analysis_notification_channel
            .immediate_publisher()
            .publish_immediate(GcodeAnalysisNotification::Cancel { job_number });
    }
}

// TODO:
// Add support for technical PN532 severe errors reporting (when can't connect to device, etc.)
impl SpoolTagObserver for ViewModel {
    fn on_tag_status(&mut self, status: &Status) {
        self.framework.borrow().undim_display();
        let ui = self.ui_weak.clone();
        // let tag_timeout = self.app_config.borrow().tag_scan_timeout;
        match status {
            Status::FoundTagNowReading => {
                ui.unwrap().global::<crate::app::AppState>().invoke_read_tag_found();
            }
            Status::FoundTagNowWriting => {
                ui.unwrap().global::<crate::app::AppState>().invoke_encode_tag_found();
            }
            Status::WriteSuccess(pure_tray_id, encoded_descriptor, cookie) => {
                let (ams_id, tray_id) = BambuPrinter::get_ams_and_tray_id(*pure_tray_id);
                let ams_id = ams_id as i32;
                let tray_id = tray_id as i32;

                if let (Ok(tag_info), Ok(encode_cookie)) = (
                    TagInformation::from_descriptor(encoded_descriptor),
                    serde_json::from_str::<EncodeCookie>(cookie),
                ) {
                    let tag_info_clone = if self.store.is_available() { Some(tag_info.clone()) } else { None };
                    if let Some(ui_spool_info) = self.tag_info_to_ui_spool_info(&tag_info) {
                        self.filament_staging.borrow_mut().set_tag_info(tag_info, StagingOrigin::Encoded);
                        ui.unwrap()
                            .global::<crate::app::AppState>()
                            .invoke_update_spool_staging(ui_spool_info, crate::app::SpoolStagingState::Encoded);
                        ui.unwrap().global::<crate::app::AppState>().invoke_encoding_succeeded(ams_id, tray_id);
                    } else {
                        ui.unwrap()
                            .global::<crate::app::AppState>()
                            .invoke_encoding_failed(SharedString::from("Descriptor Generation Error"));
                    }
                    if let Some(mut tag_info) = tag_info_clone {
                        let mut weight = None;
                        if let ScaleWeight::Stable(stable_weight) = encode_cookie.scale_weight {
                            if stable_weight != 0 {
                                // The threshold is set in SpoolEase Scale as const 5g
                                weight = Some(stable_weight);
                            }
                        }
                        if !encode_cookie.spool_id.is_empty() {
                            tag_info.id = Some(encode_cookie.spool_id);
                        }
                        if let Err(err) = self.store.try_send_op(StoreOp::WriteTag {
                            tag_info,
                            tag_operation: TagOperation::EncodeTag { weight },
                            cookie: Box::new(StoreWriteTagCookie {
                                notify_scale: false,
                                store_request_origin: StoreRequestOrigin::Encode,
                            }),
                        }) {
                            info!("Error writing tag to store : {}", err);
                        }
                    }
                }
            }
            Status::ReadSuccess(read_text) => {
                if let Ok(tag_info) = TagInformation::from_descriptor(read_text) {
                    let tag_info_clone = if self.store.is_available() { Some(tag_info.clone()) } else { None };
                    if let Some(ui_spool_info) = self.tag_info_to_ui_spool_info(&tag_info) {
                        self.filament_staging.borrow_mut().set_tag_info(tag_info, StagingOrigin::Scanned);
                        ui.unwrap().global::<crate::app::AppState>().invoke_read_tag_succeeded(ui_spool_info);
                    } else {
                        ui.unwrap()
                            .global::<crate::app::AppState>()
                            .invoke_read_tag_failed(SharedString::from("Invalid Tag Content"));
                    }
                    if let Some(tag_info) = tag_info_clone {
                        if let Err(err) = self.store.try_send_op(StoreOp::WriteTag {
                            tag_info,
                            tag_operation: TagOperation::ReadTag,
                            cookie: Box::new(StoreWriteTagCookie {
                                notify_scale: false,
                                store_request_origin: StoreRequestOrigin::Scan,
                            }),
                        }) {
                            info!("Error writing tag to store : {}", err);
                        }
                    }
                }
            }
            Status::Failure(spool_tag::Failure::TagWriteFailure) => {
                ui.unwrap().global::<crate::app::AppState>().invoke_encoding_failed("".to_shared_string());
            }
            Status::Failure(spool_tag::Failure::TagReadFailure) => {
                ui.unwrap()
                    .global::<crate::app::AppState>()
                    .invoke_read_tag_failed(SharedString::from("Error: Failed to Scan Tag"));
            }
        }
    }

    fn on_pn532_status(&mut self, status: bool) {
        self.app_config.borrow_mut().report_pn532(status);
    }

    fn on_emulated_tag_read(&mut self) {
        info!("Emulated tag scanned");
        let ui = self.ui_weak.clone();
        ui.unwrap().global::<crate::app::AppState>().invoke_emulated_tag_scanned();
    }
}

impl FrameworkObserver for ViewModel {
    fn on_web_config_started(&self, key: &str, mode: WebConfigMode) {
        let mode = match mode {
            WebConfigMode::AP => crate::app::WebConfigState::StartedAP,
            WebConfigMode::STA => {
                if self.app_config.borrow().missing_configs(false) {
                    crate::app::WebConfigState::StartedSTADisplayed
                } else {
                    crate::app::WebConfigState::StartedSTA
                }
            }
        };
        self.ui_weak
            .unwrap()
            .global::<crate::app::FrameworkState>()
            .invoke_web_config_started(SharedString::from(key), mode);
    }

    fn on_web_config_stopped(&self) {
        self.ui_weak.unwrap().global::<crate::app::FrameworkState>().invoke_web_config_stopped();
    }
    fn on_wifi_sta_connected(&self) {
        self.framework.borrow().check_firmware_ota();
    }

    fn on_ota_start(&self) {
        self.ui_weak.unwrap().global::<crate::app::FrameworkState>().invoke_ota_started();
    }

    fn on_ota_status(&self, text: &str) {
        self.ui_weak
            .unwrap()
            .global::<crate::app::FrameworkState>()
            .invoke_ota_status(SharedString::from(text));
    }

    fn on_ota_completed(&self, text: &str) {
        self.ui_weak
            .unwrap()
            .global::<crate::app::FrameworkState>()
            .invoke_ota_completed(SharedString::from(text));
    }

    fn on_ota_failed(&self, text: &str) {
        self.ui_weak
            .unwrap()
            .global::<crate::app::FrameworkState>()
            .invoke_ota_failed(SharedString::from(text));
    }

    fn on_ota_version_available(&self, version: &str, newer: bool) {
        if newer {
            info!("OTA: New version {version}");
        } else {
            info!("OTA: Up to date with available version {version}");
        }
        self.ui_weak
            .unwrap()
            .global::<crate::app::FrameworkState>()
            .invoke_set_ota_info(crate::app::OtaInfo {
                version: version.to_shared_string(),
                newer,
            });
    }

    fn on_webapp_url_update(&self, ip_url: &str, name_url: Option<&str>, ssid: &str) {
        let final_url = if let Some(name_url) = name_url {
            &format!("{ip_url} / {name_url}")
        } else {
            ip_url
        };
        self.ui_weak
            .unwrap()
            .global::<crate::app::FrameworkState>()
            .invoke_set_web_config_url(final_url.to_shared_string(), SharedString::from(ssid));
    }

    fn on_initialization_completed(&self, status: bool) {
        if status {
            term_info!(&"-".repeat(66));
            term_info!("Initialization completed successfully");
            term_info!(&"-".repeat(66));
            self.ui_weak.unwrap().global::<crate::app::AppState>().invoke_initialization_completed();
        } else {
            // TODO: This event here goes to the AppState and not to Framework, think about that.
            self.ui_weak
                .unwrap()
                .global::<crate::app::AppState>()
                .invoke_boot_failed("Boot Failed\nScroll Up for Details".to_shared_string());
            term_info!(&"x".repeat(47));
            term_info!("Initialization failed - Review errors, fix, and restart");
            term_info!(&"x".repeat(47));
        }
    }
}

struct TerminalViewModel {
    ui_weak: slint::Weak<crate::app::AppWindow>,
}

impl TerminalObserver for TerminalViewModel {
    fn on_add_text(&self, text: &str) {
        self.ui_weak
            .unwrap()
            .global::<crate::app::FrameworkState>()
            .invoke_add_term_text(text.to_shared_string());
    }
}

struct SelectedPrinter {
    printers: Vec<Rc<RefCell<BambuPrinter>>>,
    index: usize,
}

impl SelectedPrinter {
    fn new(vec: Vec<Rc<RefCell<BambuPrinter>>>, default_index: usize) -> Self {
        Self {
            printers: vec,
            index: default_index,
        }
    }
}

impl Deref for SelectedPrinter {
    type Target = Rc<RefCell<BambuPrinter>>;
    fn deref(&self) -> &Self::Target {
        &self.printers[self.index]
    }
}

impl DerefMut for SelectedPrinter {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.printers[self.index]
    }
}

impl SpoolScaleObserver for ViewModel {
    fn on_scale_loaded(&mut self, weight: i32) {
        info!("Scale loaded with {weight} g");
        self.framework.borrow().undim_display();
        self.ui_weak
            .unwrap()
            .global::<crate::app::AppState>()
            .invoke_spool_scale_loaded(weight, false);
    }

    fn on_scale_load_changed_stable(&mut self, weight: i32) {
        debug!("Scale load changed to stable {weight}");
        self.framework.borrow().undim_display();
        self.ui_weak
            .unwrap()
            .global::<crate::app::AppState>()
            .invoke_spool_scale_load_changed(weight, true);
    }

    fn on_scale_load_changed_unstable(&mut self, weight: i32) {
        debug!("Scale load changed to unstable {weight}");
        self.framework.borrow().undim_display();
        self.ui_weak
            .unwrap()
            .global::<crate::app::AppState>()
            .invoke_spool_scale_load_changed(weight, false);
    }

    fn on_scale_load_removed(&mut self) {
        debug!("Scale load removed");
        self.framework.borrow().undim_display();
        self.ui_weak.unwrap().global::<crate::app::AppState>().invoke_spool_scale_load_removed();
    }

    fn on_scale_raw_samples_avg(&mut self, raw_data: i32) {
        self.ui_weak
            .unwrap()
            .global::<crate::app::AppState>()
            .invoke_spool_scale_raw_samples_avg(raw_data);
    }

    fn on_scale_connected(&mut self) {
        debug!("Scale connected");
        self.ui_weak.unwrap().global::<crate::app::AppState>().invoke_spool_scale_connected();
    }

    fn on_scale_disconnected(&mut self) {
        debug!("Scale disconnected");
        self.ui_weak.unwrap().global::<crate::app::AppState>().invoke_spool_scale_disconnected();
    }

    fn on_scale_uncalibrated(&mut self) {
        debug!("Scale uncalibrated");
        self.ui_weak.unwrap().global::<crate::app::AppState>().invoke_spool_scale_uncalibrated();
    }

    fn on_term_text(&mut self, text: &str) {
        let text = format!("\n[S] {text}");
        self.ui_weak
            .unwrap()
            .global::<crate::app::FrameworkState>()
            .invoke_add_term_text(text.into());
    }

    fn on_tag_status(&mut self, status: &shared::spool_tag::Status) {
        SpoolTagObserver::on_tag_status(self, status);
    }

    fn on_pn532_status(&mut self, status: bool) {
        if status {
            term_info!("[S] Scale initialized the NFC module successfuly");
        } else {
            term_info!("[S] Warning: Scale failed to initialize the NFC module");
        }
    }

    fn on_button_pressed(&mut self, scale_weight: ScaleWeight) -> Option<bool> {
        let ui = self.ui_weak.unwrap();
        let ui_app_state = ui.global::<crate::app::AppState>();
        if let Some(tag_info) = self.filament_staging.borrow().tag_info() {
            match scale_weight {
                ScaleWeight::Stable(weight) => {
                    if weight == 0 {
                        info!("User Error: Reqeust to store tag with no weight on scale");
                        ui_app_state.invoke_show_spoolscale_dialog(
                            "No Weight on Scale\n\nCan't Update Spool Weight".to_shared_string(),
                            crate::app::StatusType::Error,
                        );
                        Some(false)
                    } else if let Err(err) = self.store.try_send_op(StoreOp::WriteTag {
                        tag_info: tag_info.clone(),
                        tag_operation: TagOperation::UpdateWeight { weight },
                        cookie: Box::new(StoreWriteTagCookie {
                            notify_scale: true,
                            store_request_origin: StoreRequestOrigin::UpdateWeight,
                        }),
                    }) {
                        info!("Error writing tag to store : {}", err);
                        ui_app_state.invoke_show_spoolscale_dialog(
                            "Internal Error\n\nFailed to Update Filament Weight".to_shared_string(),
                            crate::app::StatusType::Error,
                        );
                        // TODO: notify on GUI and on Scale Led
                        Some(false)
                    } else {
                        info!("Submitted internally a request to store weight");
                        None
                    }
                }
                ScaleWeight::Unstable(_) => {
                    info!("User Error: Reqeust to store tag with weight but scale weight is not stable");
                    ui_app_state.invoke_show_spoolscale_dialog(
                        "Weight on Scale Not Stable\n\nCan't Update Spool Weight".to_shared_string(),
                        crate::app::StatusType::Error,
                    );
                    Some(false)
                    // TODO: notify on GUI and on Scale Led
                }
                ScaleWeight::Unknown => {
                    info!("Software Error: scale weight unknown after connec?");
                    ui_app_state.invoke_show_spoolscale_dialog(
                        "Internal Software Error\n\nCan't Update Spool Weight".to_shared_string(),
                        crate::app::StatusType::Error,
                    );
                    Some(false)
                }
            }
        } else {
            info!("User Error: Reqeust to store tag with weight but no tag information in staging");
            ui_app_state.invoke_show_spoolscale_dialog(
                "No Spool Tag in Staging\n\nCan't Update Spool Weight".to_shared_string(),
                crate::app::StatusType::Error,
            );
            Some(false)
            // TODO:  notify on GUI and on Scale Led
        }
    }

    // note that this is from Scale (which ends up calling the GcodeAnalyzerObserver on_gcode_analysis)
    fn on_gcode_analysis(&mut self, job_number: i32, printer_index: usize, gcode_analysis: Vec<FilamentUsageEntry>) {
        let filament_usage = FilamentUsage { data: gcode_analysis };

        shared::gcode_analysis_task::GcodeAnalyzerObserver::on_gcode_analysis(self, job_number, printer_index, filament_usage);
    }
}

#[derive(Clone, Debug, PartialEq)]
enum StoreRequestOrigin {
    Scan,
    Encode,
    UpdateWeight,
}

#[derive(Clone, Debug)]
struct StoreWriteTagCookie {
    notify_scale: bool,
    store_request_origin: StoreRequestOrigin,
}

impl Cookie for StoreWriteTagCookie {}

impl StoreObserver for ViewModel {
    fn on_tag_stored(&mut self, result: Result<Option<String>, String>, cookie: Box<dyn AnyClone>) {
        if let Ok(cookie) = cookie.into_any().downcast::<StoreWriteTagCookie>() {
            let ui = self.ui_weak.unwrap();
            let ui_app_state = ui.global::<crate::app::AppState>();
            // on error we update on any failure to store using same message - for consistency
            // id == None means no database (sd card) available
            match result {
                Ok(id) => {
                    if [StoreRequestOrigin::Scan, StoreRequestOrigin::Encode].contains(&cookie.store_request_origin) {
                        if let Some(ref mut tag_info) = self.filament_staging.borrow_mut().tag_info_mut() {
                            if let Some(id) = &id {
                                tag_info.id = Some(id.clone());
                            }
                            let ui = self.ui_weak.clone();
                            if let Some(ui_spool_info) = self.tag_info_to_ui_spool_info(tag_info) {
                                ui.unwrap()
                                    .global::<crate::app::AppState>()
                                    .invoke_update_spool_staging(ui_spool_info, crate::app::SpoolStagingState::Unchanged);
                            }
                        }
                    }
                    if cookie.notify_scale {
                        if let Some(id) = &id {
                            self.spool_scale_model.borrow().button_response(true);
                            ui_app_state.invoke_show_spoolscale_dialog(
                                format!("Updated Filament Weight\n\nFor Spool {id}").into(),
                                crate::app::StatusType::Success,
                            );
                        } else {
                            self.spool_scale_model.borrow().button_response(false);
                            // We use the same UI style message on any error writing to store for consistency, so it's not really 'spoolscale' dialog
                            ui_app_state.invoke_show_spoolscale_dialog(
                                "Operation Not Allowed\n\nDatabase Missing or Unavailable".to_shared_string(),
                                crate::app::StatusType::Error,
                            );
                        }
                    }
                }
                Err(err) => {
                    if cookie.notify_scale {
                        self.spool_scale_model.borrow().button_response(false);
                    }
                    // We use the same UI style message on any error writing to store for consistency, so it's not really 'spoolscale' dialog
                    ui_app_state.invoke_show_spoolscale_dialog(
                        format!("Failed to Update Filament Weight/Tag\n\n{err}").to_shared_string(),
                        crate::app::StatusType::Error,
                    );
                }
            }
        }
    }
}

fn get_brand_from_text(text: &str) -> Option<&'static str> {
    let text = text.to_lowercase();
    for brand in FILAMENT_BRAND_NAMES.lines() {
        if brand.contains(',') {
            if let Some((keyword, brand)) = brand.split_once(',') {
                if text.contains(&keyword.to_lowercase()) {
                    return Some(brand);
                }
            }
        } else if text.contains(&brand.to_lowercase()) {
            return Some(brand);
        }
    }
    None
}

fn decode_csv_field(s: &str) -> String {
    if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        s[1..s.len() - 1].replace("\"\"", "\"")
    } else {
        s.to_string()
    }
}

#[embassy_executor::task] // up to two printers in parallel
pub async fn printers_scheduled_store_state_task(framework: Rc<RefCell<Framework>>, view_model: Rc<RefCell<ViewModel>>) {
    {
        let file_store = framework.borrow().file_store();
        let file_store = file_store.lock().await;
        if !file_store.card_installed {
            term_info!("SDCard not installed, won't restore state on restart");
            return;
        }
    }

    let num_of_printers = view_model.borrow().bambu_printer_model.printers.len();
    term_info!("Restoring printer(s) state");
    for printer_index in 0..num_of_printers {
        let printer = view_model.borrow().bambu_printer_model.printers[printer_index].clone();
        BambuPrinter::load_printer_state(&framework, &printer).await;
    }

    let mut printer_index = 0;
    let delay_time = max(1000u64, (3000 / num_of_printers) as u64); // want every printer to save every 3 seconds, and not all together
    loop {
        if printer_index < num_of_printers {
            let printer = view_model.borrow().bambu_printer_model.printers[printer_index].clone();
            BambuPrinter::store_printer_state(&framework, &printer).await;
        }
        Timer::after_millis(delay_time).await;
        printer_index += 1;
        if printer_index >= num_of_printers {
            printer_index = 0;
        }
    }
}

#[embassy_executor::task]
pub async fn store_printers_consume(view_model: Rc<RefCell<ViewModel>>) {
    info!("store_printers_consume task started");
    let store = view_model.borrow().store.clone();
    Timer::after_secs(10).await;
    loop {
        if store.is_available() {
            break;
        }
        Timer::after_secs(1).await;
    }
    if !store.is_available() {
        warn!("Store is not available in store_printer_consume_task");
        return;
    }
    //TODO: test CsvDB is available
    let num_of_printers = view_model.borrow().bambu_printer_model.printers.len();
    loop {
        for printer_index in 0..num_of_printers {
            let printer = view_model.borrow().bambu_printer_model.printers[printer_index].clone();
            let num_of_trays = printer.borrow().ams_trays().len();
            for tray_id in 0..num_of_trays {
                let spool_id;
                let consumed_during_print;
                let consumed_during_print_saved;
                {
                    let printer_borrow = printer.borrow();
                    let tray = &printer_borrow.ams_trays()[tray_id];
                    spool_id = if let Some(tag_info) = &tray.meta_info.tag_info {
                        if let Some(id) = &tag_info.id {
                            id.clone()
                        } else {
                            continue;
                        }
                    } else {
                        continue;
                    };
                    consumed_during_print = tray.meta_info.consumed_since_load;
                    if consumed_during_print == 0.0 {
                        continue;
                    }
                    consumed_during_print_saved = tray.meta_info.consumed_since_load_saved;
                    if consumed_during_print_saved == consumed_during_print {
                        continue;
                    }
                }
                let store = view_model.borrow().store.clone();
                if let Some(mut spool_rec) = store.get_spool_by_id(&spool_id) {
                    let consumption_to_add_save = consumed_during_print - consumed_during_print_saved;
                    spool_rec.consumed_since_add += consumption_to_add_save;
                    spool_rec.consumed_since_weight += consumption_to_add_save;
                    info!(
                        "Increase spool {} consumption by {:2}g to total so far {:2}g and since last weight to {:2}g",
                        spool_id, consumption_to_add_save, spool_rec.consumed_since_add, spool_rec.consumed_since_weight
                    );
                    match store.update_spool(spool_rec).await {
                        Ok(_) => {
                            // update saved in tray
                            let mut printer_borrow = printer.borrow_mut();
                            printer_borrow.update_ams_tray(tray_id, |tray| {
                                tray.meta_info.consumed_since_load_saved = tray.meta_info.consumed_since_load
                            });
                        }
                        Err(err) => {
                            error!("Error updating consumption of spool {spool_id} : {err}");
                        }
                    }
                } else {
                    error!("While updating consume data spool_id not found");
                }
            }
        }
        Timer::after_secs(1).await;
    }
}

impl GcodeAnalyzerObserver for ViewModel {
    fn on_gcode_analysis(&mut self, job_number: i32, printer_index: usize, filament_usage: FilamentUsage) {
        if let Some(printer) = self.bambu_printer_model.printers.get(printer_index) {
            let mut printer_borrow = printer.borrow_mut();
            let printer_log_id = printer_borrow.printer_number;
            info!("[{}] Setting gcode analysis with {} entries", printer_log_id, filament_usage.data.len());
            if let Some(curr_print_project) = &mut printer_borrow.curr_print_project {
                // TODO: turn to a function on print_project or on printer
                match &curr_print_project.gcode_analysis {
                    GcodeAnalysis::WaitingForPrinter => {
                        warn!("[{}>] Print monitoring awaiting printer, ignoring gcode analysis", printer_log_id);
                        return;
                    }
                    GcodeAnalysis::Requested {
                        at: _,
                        job_number: awaited_job_number,
                    }
                    | GcodeAnalysis::Received {
                        at: _,
                        job_number: awaited_job_number,
                        usage: _,
                    } => {
                        if *awaited_job_number != job_number {
                            warn!(
                                "[{}] Print monitoring awaiting job number {}, received a different job number {}, ignoring gcode analysis",
                                printer_log_id, awaited_job_number, job_number
                            );
                            return;
                        }
                    }
                }
                curr_print_project.gcode_analysis = GcodeAnalysis::Received {
                    at: Instant::now(),
                    job_number,
                    usage: filament_usage,
                };
                if curr_print_project.consume_index == -1 {
                    curr_print_project.consume_index = 0;
                }
            } else {
                error!("Internal Error setting gcode analysis to printer index {printer_index}");
            }
        }
    }

    fn on_canceled(&mut self, job_number: i32, printer_index: usize) {
        if let Some(printer) = self.bambu_printer_model.printers.get(printer_index) {
            let printer_borrow = printer.borrow();
            let printer_log_id = printer_borrow.printer_number;
            info!("[{printer_log_id}] Gcode analysis job {job_number} canceled before completion (print canceled?)");
        }
    }

    fn on_failed(&mut self, job_number: i32, printer_index: usize) {
        if let Some(printer) = self.bambu_printer_model.printers.get(printer_index) {
            let printer_borrow = printer.borrow();
            let printer_log_id = printer_borrow.printer_number;
            error!("[{printer_log_id}] Gcode analysis job {job_number} failed (exact error above?)");
        }
    }
}
