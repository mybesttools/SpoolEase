use alloc::ffi::CString;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use embassy_net::{tcp::TcpSocket, IpEndpoint, Ipv4Address};
use esp_alloc::HeapStats;
use esp_mbedtls::asynch::Session;
use esp_mbedtls::{Certificates, Mode, TlsError, TlsReference, TlsVersion};
use esp_println::println;
use framework::debug;
use snafu::prelude::*;

// Helper for using Snafu

pub struct DebugWrap<E>(pub E);

impl<E: core::fmt::Debug> core::error::Error for DebugWrap<E> {}

impl<E: core::fmt::Debug> core::fmt::Debug for DebugWrap<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.fmt(f)
    }
}

impl<E: core::fmt::Debug> core::fmt::Display for DebugWrap<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(&self.0, f)
    }
}


#[derive(Debug)]
struct ControlResponse {
    code: i32,
    string: String,
}

pub struct MyFtps<'a, T>
where
    T: Into<IpEndpoint>,
{
    control_socket: Option<TcpSocket<'a>>,
    tls: TlsReference<'a>,
    ftp_endpoint: T,
    server_name: String,
    server_certs: Certificates<'a>,
    session: Option<Session<'a, TcpSocket<'a>>>,
}

// #[snafu(display("Failed to open volume"))]
// OpenVolume {
//     #[snafu(source(from(embedded_sdmmc::asynchronous::Error<E>, DebugWrap)))]
//     source: DebugWrap<embedded_sdmmc::asynchronous::Error<E>>,
// },
#[derive(Snafu, Debug)]
pub enum Error {
    #[snafu(display("Failed to connect"))]
    Connect {
        #[snafu(source(from(embassy_net::tcp::ConnectError, DebugWrap)))]
        source: DebugWrap<embassy_net::tcp::ConnectError>,
    },
    Tls {
        #[snafu(source(from(TlsError, DebugWrap)))]
        source: DebugWrap<TlsError>,
    },
    Usage {
        reason: String,
    },
    UnexpectedEof,
    InvalidResponse,
    Ftp {
        response: ControlResponse,
    },
}

impl<'a, T> MyFtps<'a, T>
where
    T: Into<IpEndpoint> + Clone,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        control_socket: TcpSocket<'a>,
        tls: TlsReference<'a>,
        ftp_endpoint: T,
        server_name: String,
        server_certs: Certificates<'a>,
    ) -> MyFtps<'a, T>
    where
        T: Into<IpEndpoint> + Clone,
    {
        MyFtps {
            control_socket: Some(control_socket),
            tls,
            ftp_endpoint,
            server_name,
            server_certs,
            session: None,
        }
    }

    pub async fn connect(&mut self) -> Result<(), Error> {
        println!("MyFTPS: Connecting...");
        self.control_socket
            .as_mut()
            .unwrap()
            .connect(self.ftp_endpoint.clone())
            .await
            .context(ConnectSnafu)?;

        println!("MyFTPS: COnnected");
        let server_name = CString::new(self.server_name.clone()).unwrap();

        let control_socket = self.control_socket.take().unwrap();

        println!("MyFTPS: Establishing TLS...");
        let mut session = Session::new(
            control_socket,
            Mode::Client {
                servername: server_name.as_c_str(),
            },
            TlsVersion::Tls1_2,
            self.server_certs,
            self.tls,
        )
        .context(TlsSnafu)?;

        session.connect().await.context(TlsSnafu)?;

        self.session = Some(session);

        let response = self.read_decoded_control_response().await?;
        if response.code != 220 {
            return Err(Error::Ftp { response });
        }

        Ok(())
    }

    pub async fn login(&mut self, user: &str, password: &str) -> Result<bool, Error> {
        let user_response = self
            .control_transaction(&format!("USER {user}\r\n"))
            .await?;
        match user_response.code {
            230 => return Ok(true),
            331 => (),
            _ => {
                return Err(Error::Ftp {
                    response: user_response,
                })
            }
        }
        // user ok, continue to password
        let pswd_response = self
            .control_transaction(&format!("PASS {password}\r\n"))
            .await?;
        match pswd_response.code {
            230 => Ok(true),
            530 => Ok(false),
            _ => Err(Error::Ftp {
                response: user_response,
            }),
        }
    }

    pub async fn quit(&mut self) -> Result<bool, Error> {
        let quit_response = self.control_transaction("QUIT\r\n").await?;
        match quit_response.code {
            221 => Ok(true),
            _ => Err(Error::Ftp {
                response: quit_response,
            }),
        }
    }

    pub async fn noop(&mut self) -> Result<bool, Error> {
        let noop_response = self.control_transaction("NOOP\r\n").await?;
        match noop_response.code {
            220 => Ok(true),
            _ => Err(Error::Ftp {
                response: noop_response,
            }),
        }
    }

    pub async fn start_retrieve<'b>(
        &mut self,
        path: &'b str,
        mut data_socket: TcpSocket<'b>,
    ) -> Result<Session<'b, TcpSocket<'b>>, Error>
    where
        'a: 'b,
    {
        // This is a bit tricky so save memory
        // We close the control session before initiating the data session.
        // It seems printer ftp anyway close the control channel when data streaming start
        // So order of things change a bit, so at any given time only one TLS session takes memory
        // And it seems to work with P1S at least
        let response = self.control_transaction("PASV\r\n").await?;
        if response.code != 227 {
            return Err(Error::Ftp { response });
        }

        let pasv_result = Self::parse_pasv(&response.string)?;

        let response = self
            .control_transaction(&format!("RETR {path}\r\n"))
            .await?;

        data_socket
            .connect(pasv_result)
            .await
            .context(ConnectSnafu)?;

        let stats: HeapStats = esp_alloc::HEAP.stats();
        debug!("1. {}", stats);

        self.close().await?;
        self.session = None; // clear session memory before allocating new session to save memory, not exactly by ftp spec, but work

        let stats: HeapStats = esp_alloc::HEAP.stats();
        debug!("2. {}", stats);

        let server_name = CString::new(self.server_name.clone()).unwrap();
        let mut session = Session::new(
            data_socket,
            Mode::Client {
                servername: server_name.as_c_str(),
            },
            TlsVersion::Tls1_2,
            self.server_certs,
            self.tls,
        )
        .context(TlsSnafu)?;
        session.connect().await.context(TlsSnafu)?;

        let stats: HeapStats = esp_alloc::HEAP.stats();
        debug!("3. {}", stats);

        if response.code != 150 {
            session.close().await.context(TlsSnafu)?;
            return Err(Error::Ftp { response });
        }
        Ok(session)
    }

    pub async fn complete_retrieve(&mut self) -> Result<(), Error> {
        let response = self.read_decoded_control_response().await?;
        if response.code != 226 {
            return Err(Error::Ftp { response })
        }
        Ok(())
    }

    fn parse_pasv(s: &str) -> Result<(Ipv4Address, u16), Error> {
        let start = s.find('(').ok_or(Error::InvalidResponse)? + 1;
        let end = s.find(')').ok_or(Error::InvalidResponse)?;
        let parts: Vec<u8> = s[start..end]
            .split(',')
            .filter_map(|x| x.trim().parse().ok())
            .collect();

        if parts.len() != 6 {
            return Err(Error::InvalidResponse);
        }

        let ip = Ipv4Address::new(parts[0], parts[1], parts[2], parts[3]);
        let port = (parts[4] as u16) << 8 | (parts[5] as u16);
        Ok((ip, port))
    }

    async fn control_transaction(&mut self, command: &str) -> Result<ControlResponse, Error> {
        self.write_control(command).await?;
        let response = self.read_decoded_control_response().await?;
        Ok(response)
    }
    async fn write_control(&mut self, command: &str) -> Result<(), Error> {
        if let Some(session) = self.session.as_mut() {
            session.write(command.as_bytes()).await.context(TlsSnafu)?;
            session.flush().await.context(TlsSnafu)?;
        }
        Ok(())
    }

    async fn read_decoded_control_response(&mut self) -> Result<ControlResponse, Error> {
        let response_bytes = self.read_control_response().await?;
        let string = String::from_utf8(response_bytes).map_err(|_| Error::InvalidResponse)?;
        let code_str = string.split(' ').next().unwrap(); // read_control would fail if no space
        let code = code_str
            .parse::<i32>()
            .map_err(|_| Error::InvalidResponse)?;
        Ok(ControlResponse { code, string })
    }
    async fn read_control_response(&mut self) -> Result<Vec<u8>, Error> {
        if let Some(session) = self.session.as_mut() {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 64];
            let mut lines = Vec::new();

            let (mut code, mut multiline) = ([0; 3], false);

            loop {
                let n = session.read(&mut tmp).await.context(TlsSnafu)?;
                if n == 0 {
                    return Err(Error::UnexpectedEof);
                }
                buf.extend_from_slice(&tmp[..n]);

                while let Some(pos) = buf.windows(2).position(|w| w == b"\r\n") {
                    let line = buf.drain(..pos + 2).collect::<Vec<u8>>();
                    if lines.is_empty() {
                        // first line
                        if line.len() < 4 {
                            return Err(Error::InvalidResponse);
                        }
                        code.copy_from_slice(&line[..3]);
                        multiline = line[3] == b'-';
                    }
                    let is_end = line.len() >= 4 && line[..3] == code && line[3] == b' ';
                    lines.extend(line);
                    if !multiline || is_end {
                        return Ok(lines);
                    }
                }
            }
        } else {
            Err(Error::Usage {
                reason: "read_control_response called w/o session".to_string(),
            })
        }
    }

    pub async fn close(&mut self) -> Result<(), Error> {
        if self.session.is_some() {
            let mut session = self.session.take().unwrap();
            session.close().await.context(TlsSnafu)?;
        }
        Ok(())
    }
}
