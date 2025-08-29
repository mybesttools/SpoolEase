#![allow(dead_code)]
use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use embassy_time::Instant;
use framework::{debug, error, info};
use shared::{
    gcode_analysis::FilamentUsageEntry,
    gcode_analysis_task::{Fetch3mf, FilamentUsage},
};

use crate::{
    bambu::BambuPrinter,
    bambu_api::{self, AmsMapping2Entry, GcodeState},
};

const EXTRA_DEBUG: bool = true;

macro_rules! debugex {
    ($($t:tt)*) => {
        if EXTRA_DEBUG {
            debug!($($t)*);
        }
    };
}

#[derive(Debug, PartialEq)]
pub enum GcodeAnalysis {
    WaitingForPrinter,
    Requested { at: Instant, job_number: i32 },
    Received { at: Instant, job_number: i32, usage: FilamentUsage },
}

pub struct PrintProject {
    pub subtask_name: String,
    pub threemf_url: String,
    pub gcode_filename_in_3mf: String,
    pub(super) ams_mapping: Vec<i32>,
    pub(super) ams_mapping2: Option<Vec<AmsMapping2Entry>>,
    pub(super) use_ams: Option<bool>,
    pub gcode_analysis: GcodeAnalysis,

    // track printer state fields
    pub(super) total_layer_num: i32,

    //
    pub(super) need_consume: bool,
    pub consume_index: i32,
}

impl PrintProject {
    pub(super) fn new(
        subtask_name: &str,
        threemf_url: &str,
        gcode_filename_in_3mf: &str,
        ams_mapping: &[i32],
        ams_mapping2: Option<Vec<AmsMapping2Entry>>,
        use_ams: Option<bool>,
    ) -> Self {
        Self {
            subtask_name: subtask_name.to_string(),
            ams_mapping: ams_mapping.to_vec(),
            ams_mapping2,
            gcode_analysis: GcodeAnalysis::WaitingForPrinter,
            total_layer_num: -1,
            need_consume: false,
            consume_index: -1,
            threemf_url: threemf_url.to_string(),
            gcode_filename_in_3mf: gcode_filename_in_3mf.to_string(),
            use_ams,
        }
    }

    pub(super) fn get_ams_id(&self, filament_id: i32) -> Option<i32> {
        if filament_id >= 0 {
            let ams_slot = self.ams_mapping.get(filament_id as usize).copied();
            // external spool handlig is a bit complex to be prepared to deal both with case of
            // multiextruder in the future (that's the first part)
            // and single extruded (so no_ams means external spool, and not always there is ams_mapping2,
            // and sometimes even ams_mapping is empty in case of external spool (Orca on A1)
            if ams_slot.is_none() || ams_slot == Some(-1) {
                if let Some(ams_mapping2) = &self.ams_mapping2 {
                    if let Some(ams2_info) = ams_mapping2.get(filament_id as usize) {
                        if ams2_info.ams_id == 255 && ams2_info.slot_id == 0 {
                            return Some(254);
                        }
                    }
                } else if self.use_ams == Some(false) {
                    return Some(254);
                }
            }
            ams_slot
        } else {
            None
        }
    }
    pub(super) fn curr_usage_entry(&self) -> Option<&FilamentUsageEntry> {
        if self.consume_index < 0 {
            return None;
        }
        if let GcodeAnalysis::Received { at: _, job_number: _, usage } = &self.gcode_analysis {
            usage.data.get(self.consume_index as usize)
        } else {
            None
        }
    }
}

impl BambuPrinter {
    #[allow(non_snake_case)]
    pub fn process_print_message__project_file(&mut self, print: &bambu_api::PrintData) -> bool {
        let mut changed = false;
        let printer_log_id = self.printer_number;
        if !self.track_print_consume {
            info!("[{printer_log_id}] Print project started but configured not to track print filament usage");
            return false;
        }
        // TODO: theoretically all are options so could 'take' instead of clone
        if let (Some(subtask_name), Some(ams_mapping), Some(url), Some(param)) = (&print.subtask_name, &print.ams_mapping, &print.url, &print.param) {
            let ams_mapping2 = print.ams_mapping2.clone();
            let use_ams = print.use_ams;
            info!("[{printer_log_id}] Print project started: name: '{subtask_name}', using ams slots: {ams_mapping:?}, {ams_mapping2:?}");
            let mut curr_print_project = PrintProject::new(subtask_name, url, param, ams_mapping, ams_mapping2, use_ams);
            // in case of http can already fetch now and not wait for printer to download first
            if self.fetch_3mf == Fetch3mf::CloudHttp
                || curr_print_project.threemf_url.starts_with("ftp://")
                || curr_print_project.threemf_url.starts_with("file://")
            {
                let job_number = self.notify_request_gcode_analysis(&curr_print_project);
                curr_print_project.gcode_analysis = GcodeAnalysis::Requested {
                    at: Instant::now(),
                    job_number,
                };
            }
            self.curr_print_project = Some(curr_print_project);

            // set trays used in print, but first clear all

            for tray_id in 0..self.ams_trays().len() {
                self.update_ams_tray(tray_id, |tray| tray.meta_info.used_in_print = false);
            }
            self.update_virt_tray(|tray| tray.meta_info.used_in_print = false);

            for tray_id in ams_mapping {
                let tray_id = *tray_id as usize;
                if (0..self.ams_trays().len()).contains(&tray_id) {
                    self.update_ams_tray(tray_id, |tray| tray.meta_info.used_in_print = true);
                    changed = true;
                }
            }

            if let Some(ams_mapping2) = &print.ams_mapping2 {
                for ams2_info in ams_mapping2 {
                    if ams2_info.ams_id == 255 && ams2_info.slot_id == 0 {
                        self.update_virt_tray(|tray| tray.meta_info.used_in_print = true);
                        changed = true;
                    }
                }
            } else if use_ams == Some(false) {
                self.update_virt_tray(|tray| tray.meta_info.used_in_print = true);
                changed = true;
            }
        }

        changed
    }

    #[allow(non_snake_case)]
    pub fn process_print_message__print_project_logic(&mut self, print: &bambu_api::PrintData) -> bool {
        let mut changed = false;
        let mut gcode_state_change = false;
        let mut new_gcode_state = self.gcode_state;
        // Order when printing is: tray_pre, then tray_tar, then tray_now
        debugex!(">>>>> In print_project_logic");
        // let mut curr_gcode_state = GcodeState::Unknown;
        let printer_log_id = self.printer_number;

        let mut curr_print_project = self.curr_print_project.take();

        if let Some(curr_print_project) = &mut curr_print_project {
            // debugex!(">>>>> curr_print_project available");
            let mut tray_tar_change_from_tray_now = false; // plan to switch filament
            let mut new_tray_now = 255;
            let mut tray_now_change = false; // new filament is loaded
                                             // let mut tray_pre_change_to_tray_now = false; // meaning, starting to pull out filament

            let mut layer_num_change = false;
            let mut new_layer_num = self.layer_num;

            // Update print project state
            if let Some(gcode_state) = print.gcode_state {
                if gcode_state == GcodeState::Unsupported {
                    error!("[{}] Unsupported gcode state", self.printer_index);
                } else if gcode_state != self.gcode_state {
                    gcode_state_change = true;
                    new_gcode_state = gcode_state;
                }
            }

            if gcode_state_change && new_gcode_state == GcodeState::PREPARE {
                if let Some(subtak_name) = &print.subtask_name {
                    // This fix special characters (in the prepare the printer notifies of the real file name after special chars fix)
                    // This is important for FTP access
                    curr_print_project.subtask_name = subtak_name.clone();
                }
            }

            if let Some(total_layer_num) = print.total_layer_num {
                curr_print_project.total_layer_num = total_layer_num;
            }

            if let Some(layer_num) = print.layer_num {
                if layer_num != self.layer_num {
                    layer_num_change = true;
                    new_layer_num = layer_num;
                }
            }

            if let Some(ams) = &print.ams {
                if let Some(update_tray_tar) = ams.tray_tar {
                    if update_tray_tar != self.tray_tar && update_tray_tar != self.tray_now {
                        tray_tar_change_from_tray_now = true;
                    }
                }
                if let Some(update_tray_now) = ams.tray_now {
                    if update_tray_now != self.tray_now {
                        tray_now_change = true;
                    }
                    new_tray_now = update_tray_now;
                }
                // if let Some(update_tray_pre) = ams.tray_pre {
                //     if update_tray_pre != self.tray_pre && update_tray_pre == self.tray_now {
                //         tray_pre_change_to_tray_now = true;
                //     }
                // }
            }

            // Non modifying state (except consume) actions based on current state and changes
            // Notes: verifying match to layer number and color since several changes in sequence could
            //   want to consume the same usage because they come one after another and don't want
            //   to rely on order (e.g. layer change and filament change which could happen or not after)
            // if tray_pre_change_to_tray_now {
            //     // debugex!(">>>>> tray_pre_change_to_tray_now && need_consume");
            //     changed |= self.try_consume(curr_print_project);
            // }

            if tray_tar_change_from_tray_now && curr_print_project.consume_index != 0 {
                debugex!(">>>>> tray_tar_change_from_tray_now && && not first consume");
                changed |= self.try_consume(curr_print_project, ConsumeType::FilamentSwitch);
            }

            if tray_now_change {
                debugex!(">>>>> tray_now_change (from {} to {})", self.tray_now, new_tray_now);
                changed |= self.try_consume(curr_print_project, ConsumeType::FilamentSwitch);
            }

            if layer_num_change && new_layer_num != 0 {
                // important that it comes last
                debugex!(">>>>> layer_num_change (from {} to {})", self.layer_num, new_layer_num);
                changed |= self.try_consume(curr_print_project, ConsumeType::LayerChange(new_layer_num));
                // do some validations here:
                //    verify that the new entry is for the next layer
                //    if not consumed verify that previous color is different from next layer (so color change caused a consume)
            }

            if gcode_state_change && new_gcode_state == GcodeState::FINISH {
                // if there is one to consume consume it.
                // !!! verify it is the last one
                self.try_consume(curr_print_project, ConsumeType::Finish);
                info!("[{printer_log_id}] Print project finished successfuly");
                if let GcodeAnalysis::Received { at: _, job_number: _, usage } = &curr_print_project.gcode_analysis {
                    if curr_print_project.consume_index != usage.data.len() as i32 {
                        error!("[{printer_log_id}] Print project filament consumption tracking didn't finish well, reached index {} (0 based), while usage data contain {} records", curr_print_project.consume_index, usage.data.iter().len());
                    } else {
                        info!("[{printer_log_id}] All consumption entries used as expected");
                    }
                } else {
                    error!("[{printer_log_id}] Something is wrong tracking print project, at FINISH no gcode_analysis data available");
                }
            }

            if gcode_state_change && new_gcode_state == GcodeState::FAILED {
                info!("[{printer_log_id}] Print project failed");
            }

            // Modifying state actions

            if layer_num_change && new_gcode_state == GcodeState::RUNNING {
                // here we don't test for gcode_state_change becasue it's the ongoing state that counts
                // debugex!(">>>>> layer_num_change, set need_consume true");
                curr_print_project.need_consume = true;
            }

            if tray_now_change && new_tray_now != 255 {
                // debugex!(">>>>> tray_now_change, set need_consume true");
                curr_print_project.need_consume = true;
            }

            // all the gcode_state, layer_num, tray_tar/pre/now will be updated by process_ams since it is not only for print case

            // curr_gcode_state = curr_print_project.gcode_state;

            // // debugex!(">>>> {:?}, {:?}, {}", curr_print_project.gcode_analysis, curr_print_project.gcode_state, curr_print_project.layer_num);
            // if curr_print_project.gcode_analysis == GcodeAnalysis::WaitingForPrinter
            //     && curr_print_project.gcode_state == GcodeState::RUNNING
            //     && curr_print_project.total_layer_num != -1
            // {
            //     // curr_print_project.need_consume = true; // changed to set it only when state CHANGED to running
            // }

            // actions post updates,
            // be aware that still can't rely on updated self.tray_tar/now/pre which will be updated later

            // debug!(">>>>> gcode_state_change {gcode_state_change}, new_gcode_state {new_gcode_state:?}, curr_print_project.gcode_analysis {:?}", curr_print_project.gcode_analysis);
            if gcode_state_change && new_gcode_state == GcodeState::RUNNING {
                // if not requested earlier, request scale to fetch gcode from printer and analyze it
                // In case of ftp it will be requested here, if http already earlier when project_file arrived
                if curr_print_project.gcode_analysis == GcodeAnalysis::WaitingForPrinter {
                    let job_number = self.notify_request_gcode_analysis(curr_print_project);
                    curr_print_project.gcode_analysis = GcodeAnalysis::Requested {
                        at: Instant::now(),
                        job_number,
                    };
                }
                curr_print_project.need_consume = true;
            }

            if gcode_state_change && new_gcode_state == GcodeState::FAILED && curr_print_project.gcode_analysis != GcodeAnalysis::WaitingForPrinter {
                match curr_print_project.gcode_analysis {
                    GcodeAnalysis::WaitingForPrinter => unreachable!(),
                    GcodeAnalysis::Requested { at: _, job_number } | GcodeAnalysis::Received { at: _, job_number, usage: _ } => {
                        self.notify_cancel_gcode_analysis(job_number);
                    }
                }
            }
        }

        self.curr_print_project = curr_print_project;

        // need to do it here because above we used 'take()' to get curr_print_project and only in here in previous line we gave it back
        if gcode_state_change && [GcodeState::FAILED, GcodeState::FINISH].contains(&new_gcode_state) {
            self.curr_print_project = None;

            for tray_id in 0..self.ams_trays().len() {
                self.update_ams_tray(tray_id, |tray| tray.meta_info.used_in_print = false);
            }
            self.update_virt_tray(|tray| tray.meta_info.used_in_print = false);
        }

        changed
    }

    fn try_consume(&mut self, print_project: &mut PrintProject, consume_type: ConsumeType) -> bool {
        debugex!(">>>>>>> Trying to consume");
        let printer_log_id = self.printer_number;
        if !print_project.need_consume {
            return false;
        }
        let mut consumed = false;
        debugex!(">>>> need consume = true");
        match consume_type {
            ConsumeType::LayerChange { .. } | ConsumeType::Finish => {
                let up_to_layer_num = match consume_type {
                    ConsumeType::LayerChange(v) => v,
                    ConsumeType::Finish => -1,
                    ConsumeType::FilamentSwitch => unreachable!(),
                };
                debugex!(">>>>> Layer change consume");
                loop {
                    debugex!(">>>>> Consume loop");
                    if let Some(usage_entry) = print_project.curr_usage_entry() {
                        debugex!(">>>>>> Checking curr usage entry {usage_entry:?}");
                        if usage_entry.layer < up_to_layer_num || up_to_layer_num == -1 {
                            // comparing with previous layer - to consume all previous layers in case of skip
                            if let Some(usage_entry_tray_id) = print_project.get_ams_id(usage_entry.gcode_filament_id) {
                                self.update_any_tray(usage_entry_tray_id as usize, |ams_tray| {
                                    ams_tray.meta_info.consumed_since_load += usage_entry.weight_g;
                                    ams_tray.meta_info.consumed_since_weight += usage_entry.weight_g; 
                                    debug!(
                                        "[{printer_log_id}] Print project consumed entry {} on layer change : {:.2}g, from filament at slot {} to a session total of {:.2}g",
                                        print_project.consume_index, usage_entry.weight_g, usage_entry_tray_id, ams_tray.meta_info.consumed_since_load
                                    );
                                });
                            } else {
                                error!(
                                    "[{printer_log_id}] Internal Error? No AMS slot for gcode filament id {}",
                                    usage_entry.gcode_filament_id
                                );
                            }
                            print_project.consume_index += 1;
                            consumed = true;
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                }
                print_project.need_consume = false;
            }
            ConsumeType::FilamentSwitch => {
                if let Some(usage_entry) = print_project.curr_usage_entry() {
                    debugex!(">>>>>> Checking curr usage entry {usage_entry:?}");
                    if let Some(usage_entry_tray_id) = print_project.get_ams_id(usage_entry.gcode_filament_id) {
                        debugex!(">>>>> self.layer_num = {}", self.layer_num);
                        debugex!(">>>>> self.tray_now = {}", self.tray_now);
                        debugex!(">>>>> usage_entry_tray_id = {usage_entry_tray_id}");
                        if self.layer_num == usage_entry.layer
                            && self.tray_now == usage_entry_tray_id
                            && (0..self.ams_trays().len() as i32).contains(&usage_entry_tray_id)
                        {
                            self.update_any_tray(usage_entry_tray_id as usize, |ams_tray| {
                                    ams_tray.meta_info.consumed_since_load += usage_entry.weight_g;
                                    ams_tray.meta_info.consumed_since_weight += usage_entry.weight_g;
                                    debug!(
                                        "[{printer_log_id}] Print project consumed entry {} on filament change : {:.2}g, from filament at slot {} to a session total of {:.2}g",
                                        print_project.consume_index, usage_entry.weight_g, usage_entry_tray_id, ams_tray.meta_info.consumed_since_load
                                    );
                                });
                            print_project.consume_index += 1;
                            consumed = true;
                            print_project.need_consume = false;
                        } else {
                            // No matching data to consume, this is ok
                        }
                    } else {
                        error!("[{printer_log_id}] No matching AMS slot for usage information {:?}", usage_entry);
                    }
                } else {
                    // Could happen in the last entry
                }
            }
        }
        consumed
    }

    pub fn notify_cancel_gcode_analysis(&mut self, job_number: i32) {
        let mut observers = self.observers.clone(); // to avoid two references - can probably optimize in various ways
        for weak_observer in observers.iter_mut() {
            let observer = weak_observer.upgrade().unwrap();
            observer.borrow_mut().on_cancel_gcode_analysis(job_number);
        }
    }

    pub fn notify_request_gcode_analysis(&mut self, print_project: &PrintProject) -> i32 {
        let mut observers = self.observers.clone(); // to avoid two references - can probably optimize in various ways
        let mut job_number = 0;
        for weak_observer in observers.iter_mut() {
            let observer = weak_observer.upgrade().unwrap();
            let job_number_update = observer.borrow_mut().on_request_gcode_analysis(self, print_project);
            if job_number_update != 0 {
                if job_number == 0 {
                    job_number = job_number_update;
                } else {
                    error!(
                        "[{}] Internal software error, two gcode analysis requests listeners with with job_number, only one possible",
                        self.printer_number
                    );
                }
            }
        }
        job_number
    }
}

#[derive(PartialEq)]
enum ConsumeType {
    LayerChange(i32),
    FilamentSwitch,
    Finish,
}
