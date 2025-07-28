use core::cell::RefCell;

use alloc::{format, rc::Rc, string::String};
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, channel::Channel};
use embassy_net::{tcp::TcpSocket, Ipv4Address};
use embassy_time::{Duration, Instant};
use framework::{debug, error, info, prelude::Framework};

use crate::{bambu::bambu_print::FilamentUsage, gcode_analysis::GcodeFilamentCalc, my_ftp::MyFtps};


pub type GcodeAnalysisRequestChannel = Channel<NoopRawMutex, GcodeAnalysisRequest, 5>;

pub struct GcodeAnalysisRequest {
    pub ip: Ipv4Address,
    pub serial: String,
    pub access_code: String,
    pub subtask_name: String,
    pub plate_idx: u32,
    pub printer_index: usize,
    pub printer_number: usize,
}

pub trait GcodeAnalyzerObserver {
    fn on_gcode_analysis(&mut self, printer_index: usize, filament_usage: FilamentUsage);
}

#[embassy_executor::task]
pub async fn fetch_gcode_analysis_task(
    framework: Rc<RefCell<Framework>>,
    channel: Rc<GcodeAnalysisRequestChannel>,
    observer: alloc::rc::Weak<RefCell<dyn GcodeAnalyzerObserver>>,
) {
    let stack = framework.borrow().stack;
    let tls = framework.borrow().tls;
    info!("Started fetch_gcode_analysis task");
    let receiver = channel.receiver();
    // let receiver = {
    //     let view_model_borrow = view_model.borrow();
    //     view_model_borrow.gcode_analysis_request_channel.clone()
    // };

    'wait_for_request: loop {
        info!("Waiting for analysis request");
        let gcode_analysis_request = receiver.receive().await;
        info!("Receuved gcode analysis request");
        // Timer::after_secs(60).await;// TODO: proper handling
        let printer_log_id = gcode_analysis_request.printer_number;

        let ip = gcode_analysis_request.ip;
        let serial = gcode_analysis_request.serial;
        let access_code = gcode_analysis_request.access_code;
        let subtask_name = gcode_analysis_request.subtask_name;
        let plate_idx = gcode_analysis_request.plate_idx;
        let printer_index = gcode_analysis_request.printer_index;

        let mut data_rx_buffer = alloc::vec![0u8;16384];
        let mut data_tx_buffer = alloc::vec![0u8;1024];
        let mut data_socket = TcpSocket::new(stack, &mut data_rx_buffer, &mut data_tx_buffer);
        data_socket.set_timeout(Some(Duration::from_secs(10)));

        let mut control_rx_buffer = alloc::vec![0u8;4096];
        let mut control_tx_buffer = alloc::vec![0u8;1024];
        let mut control_socket = TcpSocket::new(stack, &mut control_rx_buffer, &mut control_tx_buffer);
        control_socket.set_timeout(Some(Duration::from_secs(10)));

        let ftp_endpoint = (ip, 990);

        let mut ftps = MyFtps::new(
            control_socket,
            tls,
            ftp_endpoint,
            serial,
            esp_mbedtls::Certificates {
                ca_chain: esp_mbedtls::X509::pem(concat!(include_str!("./certs/bambulab.pem"), "\0").as_bytes()).ok(),
                ..Default::default()
            },
        );

        info!("[{printer_log_id}] Connecting to printer ftp");
        match ftps.connect().await {
            Ok(_) => {
                info!("[{printer_log_id}] Connected to printer ftp");
            }
            Err(err) => {
                error!("[{printer_log_id}] Error connecting to printer ftp : {err}");
                continue 'wait_for_request;
            }
        }

        match ftps.login("bblp", &access_code).await {
            Ok(success) => {
                if success {
                    info!("[{printer_log_id}] Login to printer ftp succeeded");
                } else {
                    error!("[{printer_log_id}] Login to printer ftp failed");
                    continue 'wait_for_request;
                }
            }
            Err(err) => {
                error!("[{printer_log_id}] Error in login to printer ftp: {:?}", err);
                continue 'wait_for_request;
            }
        }

        // it looks like in the gcode file name (not in the bbl file name) bambu uses for gcode filename the text until "." in case there is such
        let bambu_gcode_filename_start = subtask_name.split('.').next().unwrap();
        let gcode_file_name = format!("/cache/{}_plate_{}.gcode", bambu_gcode_filename_start, plate_idx);

        let mut buf = alloc::vec![0;16384];
        let mut gcode_calc = GcodeFilamentCalc::new();
        let mut total_read = 0;
        match ftps.start_retrieve(&gcode_file_name, data_socket).await {
            Ok(mut data_session) => {
                let mut last_noop = Instant::now();
                loop {
                    match data_session.read(&mut buf).await {
                        Ok(n) => {
                            if n == 0 {
                                break;
                            }
                            total_read += n;
                            debug!(">>>>> read from ftp {total_read}");
                            match gcode_calc.add_buffer(&buf[..n]) {
                                Ok(_) => (),
                                Err(err) => {
                                    error!("[{printer_log_id}] Error while processing gcode data in file {gcode_file_name} {err:?}");
                                    continue 'wait_for_request;
                                }
                            }
                        }
                        Err(err) => {
                            error!("[{printer_log_id}] Error while reading gcode file {gcode_file_name} {err:?}");
                            continue 'wait_for_request;
                        }
                    };
                    if last_noop.elapsed() > Duration::from_secs(30) {
                        if let Err(err) = ftps.noop().await {
                            error!("[{printer_log_id}] Error sending NOOP to ftp, ignoring : {err:?}");
                        }
                        last_noop = Instant::now();
                    }
                }
                gcode_calc.done();
                if let Err(err) = ftps.complete_retrieve().await {
                    error!("[{printer_log_id}] Error completing ftp get (ignoring) : {err:?}");
                }
                if let Err(err) = ftps.quit().await {
                    error!("[{printer_log_id}] Error quitting ftp (ignoring) : {err:?}");
                }
                if let Err(err) = ftps.close().await {
                    error!("[{printer_log_id}] Error closing ftp sessio (ignoring) : {err:?}");
                }
            }
            Err(err) => {
                error!("[{printer_log_id}] Error initiating retrieve of gcode file {gcode_file_name} {err:?}");
                continue 'wait_for_request;
            }
        };

        info!("[{printer_log_id}] Completed reading and processing gcode file {gcode_file_name}");

        observer
            .upgrade()
            .unwrap()
            .borrow_mut()
            .on_gcode_analysis(printer_index, FilamentUsage::new(gcode_calc.layers_extruded));
    }
}
