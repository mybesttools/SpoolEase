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

#[derive(Debug, PartialEq)]
pub enum GcodeAnalysis {
    WaitingForPrinter,
    Requested { at: Instant, job_number: i32 },
    Received { at: Instant, job_number: i32, usage: FilamentUsage },
}

pub struct PrintProject {
    pub subtask_name: String,
    pub plate_idx: u32,
    pub threemf_url: String,
    pub gcode_filename_in_3mf: String,
    pub(super) ams_mapping: Vec<i32>,
    pub(super) ams_mapping2: Vec<AmsMapping2Entry>,
    pub gcode_analysis: GcodeAnalysis,

    // track printer state fields
    pub(super) gcode_state: GcodeState,
    pub(super) layer_num: i32,
    pub(super) total_layer_num: i32,

    //
    pub(super) need_consume: bool,
    pub consume_index: i32,
}

impl PrintProject {
    pub(super) fn new(
        subtask_name: &str,
        plate_idx: u32,
        threemf_url: &str,
        gcode_filename_in_3mf: &str,
        ams_mapping: &[i32],
        ams_mapping2: &[AmsMapping2Entry],
    ) -> Self {
        Self {
            subtask_name: subtask_name.to_string(),
            plate_idx,
            ams_mapping: ams_mapping.to_vec(),
            ams_mapping2: ams_mapping2.to_vec(),
            gcode_analysis: GcodeAnalysis::WaitingForPrinter,
            gcode_state: GcodeState::Unknown,
            layer_num: -1,
            total_layer_num: -1,
            need_consume: false,
            consume_index: -1,
            threemf_url: threemf_url.to_string(),
            gcode_filename_in_3mf: gcode_filename_in_3mf.to_string(),
        }
    }

    pub(super) fn get_ams_id(&self, filament_id: i32) -> Option<i32> {
        if filament_id >= 0 {
            self.ams_mapping.get(filament_id as usize).copied()
        } else {
            None
        }
    }
    pub(super) fn curr_usage_entry(&self) -> Option<&FilamentUsageEntry> {
        if self.consume_index < 0 {
            return None;
        }
        if let GcodeAnalysis::Received { at: _, job_number: _, usage } = &self.gcode_analysis {
            debug!("$$$$$ GcodeAnalysis received, consume_index {}/{}", self.consume_index, usage.data.len());
            usage.data.get(self.consume_index as usize)
        } else {
            None
        }
    }
}

impl BambuPrinter {
    #[allow(non_snake_case)]
    pub fn process_print_message__project_file(&mut self, print: &bambu_api::PrintData) -> bool {
        let printer_log_id = self.printer_number;
        if !self.track_print_consume {
            info!("[{printer_log_id}] Print project started but configured not to track print filament usage");
            return false;
        }
        // TODO: theoretically all are options so could 'take' instead of clone
        if let (Some(subtask_name), Some(plate_idx), Some(ams_mapping), Some(ams_mapping2), Some(url), Some(param)) = (
            &print.subtask_name,
            print.plate_idx,
            &print.ams_mapping,
            &print.ams_mapping2,
            &print.url,
            &print.param,
        ) {
            info!("[{printer_log_id}] Print project started: name: '{subtask_name}', plate: {plate_idx}, using ams slots: {ams_mapping:?}, {ams_mapping2:?}");
            let mut curr_print_project = PrintProject::new(subtask_name, plate_idx, url, param, ams_mapping, ams_mapping2);
            // in case of http can already fetch now and not wait for printer to download first
            if self.fetch_3mf == Fetch3mf::CloudHttp {
                let job_number = self.notify_request_gcode_analysis(&curr_print_project);
                curr_print_project.gcode_analysis = GcodeAnalysis::Requested {
                    at: Instant::now(),
                    job_number,
                };
            }
            self.curr_print_project = Some(curr_print_project);
        }

        false
    }

    #[allow(non_snake_case)]
    pub fn process_print_message__print_project_logic(&mut self, print: &bambu_api::PrintData) -> bool {
        let mut changed = false;
        // Order when printing is: tray_pre, then tray_tar, then tray_now
        // debug!(">>>>> In print_project_logic");
        let mut curr_gcode_state = GcodeState::Unknown;

        let mut curr_print_project = self.curr_print_project.take();

        if let Some(curr_print_project) = &mut curr_print_project {
            // debug!(">>>>> curr_print_project available");
            let mut layer_num_change = false;
            let mut tray_tar_change_from_tray_now = false; // plan to switch filament
            let mut tray_now_change = false; // new filament is loaded
                                             // let mut tray_pre_change_to_tray_now = false; // meaning, starting to pull out filament
            let mut gcode_state_change = false;
            let mut new_tray_now = 255;
            let mut new_layer_num = curr_print_project.layer_num;
            let mut new_gcode_state = curr_print_project.gcode_state;

            // Update print project state
            if let Some(gcode_state) = print.gcode_state {
                if gcode_state == GcodeState::Unsupported {
                    error!("[{}] Unsupported gcode state", self.printer_index);
                } else {
                    gcode_state_change = true;
                    new_gcode_state = gcode_state;
                }
            }

            if let Some(total_layer_num) = print.total_layer_num {
                curr_print_project.total_layer_num = total_layer_num;
            }

            if let Some(layer_num) = print.layer_num {
                if layer_num != curr_print_project.layer_num {
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
            //     // debug!(">>>>> tray_pre_change_to_tray_now && need_consume");
            //     changed |= self.try_consume(curr_print_project);
            // }

            if tray_tar_change_from_tray_now && curr_print_project.consume_index != 0 {
                // debug!(">>>>> tray_tar_change_from_tray_now && need_consume && not first layer");
                changed |= self.try_consume(curr_print_project);
            }

            if tray_now_change {
                // debug!(">>>>> tray_now_change && need_consume");
                changed |= self.try_consume(curr_print_project);
            }

            if layer_num_change {
                // important that it comes last
                // debug!(">>>>> layer_num_change");
                changed |= self.try_consume(curr_print_project);
                // do some validations here:
                //    verify that the new entry is for the next layer
                //    if not consumed verify that previous color is different from next layer (so color change caused a consume)
            }

            if gcode_state_change && new_gcode_state == GcodeState::FINISH {
                // if there is one to consume consume it.
                // !!! verify it is the last one
                self.try_consume(curr_print_project);
                info!("Print project finished successfuly");
                if let GcodeAnalysis::Received { at: _, job_number: _, usage } = &curr_print_project.gcode_analysis {
                    if curr_print_project.consume_index != usage.data.len() as i32 {
                        error!("Print project filament consumption tracking didn't finish well, reached index {} (0 based), while usage data contain {} records", curr_print_project.consume_index, usage.data.iter().len());
                    }
                } else {
                    error!("Something is wrong tracking print project, at FINISH no gcode_analysis data available");
                }
            }
            if gcode_state_change && new_gcode_state == GcodeState::FAILED {
                info!("Print project failed");
            }

            // Modifying state actions

            if layer_num_change && curr_print_project.gcode_state == GcodeState::RUNNING {
                // debug!(">>>>> layer_num_change, set need_consume true");
                curr_print_project.need_consume = true;
            }

            if tray_now_change && new_tray_now != 255 {
                // debug!(">>>>> tray_now_change, set need_consume true");
                curr_print_project.need_consume = true;
            }

            // Update all new values
            curr_print_project.gcode_state = new_gcode_state;
            curr_print_project.layer_num = new_layer_num;
            // all the tray_tar/pre/now will be updated by process_ams since it is not only for print case

            curr_gcode_state = curr_print_project.gcode_state;

            // // debug!(">>>> {:?}, {:?}, {}", curr_print_project.gcode_analysis, curr_print_project.gcode_state, curr_print_project.layer_num);
            // if curr_print_project.gcode_analysis == GcodeAnalysis::WaitingForPrinter
            //     && curr_print_project.gcode_state == GcodeState::RUNNING
            //     && curr_print_project.total_layer_num != -1
            // {
            //     // curr_print_project.need_consume = true; // changed to set it only when state CHANGED to running
            // }

            // actions post updates,
            // be aware that still can't rely on updated self.tray_tar/now/pre which will be updated later

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

            if curr_print_project.gcode_state == GcodeState::FAILED && curr_print_project.gcode_analysis != GcodeAnalysis::WaitingForPrinter {
                match curr_print_project.gcode_analysis {
                    GcodeAnalysis::WaitingForPrinter => (),
                    GcodeAnalysis::Requested { at: _, job_number } | GcodeAnalysis::Received { at: _, job_number, usage: _ } => {
                        self.notify_cancel_gcode_analysis(job_number);
                    }
                }
            }
        }

        self.curr_print_project = curr_print_project;

        if [GcodeState::FAILED, GcodeState::FINISH].contains(&curr_gcode_state) {
            self.curr_print_project = None;
        }

        changed
    }

    fn try_consume(&mut self, print_project: &mut PrintProject) -> bool {
        // debug!(">>>>>>> Trying to consume");
        let mut consumed = false;
        if print_project.need_consume {
            // debug!(">>>> need consume = true");
            if let Some(usage_entry) = print_project.curr_usage_entry() {
                // debug!(">>>>>> Getting curr usage entry {usage_entry:?}");
                if let Some(usage_entry_tray_id) = print_project.get_ams_id(usage_entry.gcode_filament_id) {
                    // debug!(">>>>> usage_entry_tray_id = {usage_entry_tray_id}");
                    if print_project.layer_num == usage_entry.layer
                        && self.tray_now == usage_entry_tray_id
                        && (0..self.ams_trays().len() as i32).contains(&usage_entry_tray_id)
                    {
                        self.update_ams_tray(usage_entry_tray_id as usize, |ams_tray| {
                            ams_tray.meta_info.consumed_since_load += usage_entry.weight_g;
                            debug!(
                                "Print project consumed consumed {:.2}g, from filament at slot {} to a total of {:.2}g",
                                usage_entry.weight_g, usage_entry_tray_id, ams_tray.meta_info.consumed_since_load
                            );
                        });
                        consumed = true;
                        print_project.consume_index += 1;
                        print_project.need_consume = false;
                    } else {
                        // No matching data to consume, this is ok
                    }
                } else {
                    error!("No matching AMS slot for usage information {:?}", usage_entry);
                }
            } else {
                // Could happen in the last entry
            }
        } else {
            // Can happen if consumed by a previous event
        }
        consumed
    }
}
