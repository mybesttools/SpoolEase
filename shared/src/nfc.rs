use deku::{DekuContainerRead, DekuContainerWrite, DekuError};
use embassy_time::Duration;

use crate::pn532_ext::{ensure_tag_formatted, process_ntag_write_long, Esp32TimerAsync};

#[derive(Debug)]
pub enum Error<E: core::fmt::Debug> {
    Pn532ExtError(crate::pn532_ext::Error<E>),
    #[allow(dead_code)]
    NdefReadError(DekuError),
    NotNdefFormatted,
}

impl<E: core::fmt::Debug> From<crate::pn532_ext::Error<E>> for Error<E> {
    fn from(v: crate::pn532_ext::Error<E>) -> Self {
        Error::Pn532ExtError(v)
    }
}

pub async fn erase_ndef_tag<I>(
    pn532: &mut pn532::Pn532<I, Esp32TimerAsync>,
    timeout: Duration,
) -> Result<(), Error<I::Error>>
where
    I: pn532::Interface,
{
    ensure_tag_formatted(pn532, timeout).await?;
    let empty_page_4 = [0x03, 0x00, 0xFE, 0x00];
    process_ntag_write_long(pn532, &empty_page_4, 4, timeout).await?;
    Ok(())
}

#[allow(dead_code)]
pub async fn write_ndef_text_record<I>(
    pn532: &mut pn532::Pn532<I, Esp32TimerAsync>,
    text: &str,
    timeout: Duration,
) -> Result<(), Error<I::Error>>
where
    I: pn532::Interface,
{
    let a_record = crate::ndef::Record::new_text_record_en(text);
    let ndef_struct = crate::ndef::NDEFStructure::new(a_record);
    ensure_tag_formatted(pn532, timeout).await?;
    process_ntag_write_long(pn532, &ndef_struct.to_bytes().unwrap(), 4, timeout).await?;
    Ok(())
}

pub async fn write_ndef_url_record<I>(
    pn532: &mut pn532::Pn532<I, Esp32TimerAsync>,
    url: &str,
    timeout: Duration,
) -> Result<(), Error<I::Error>>
where
    I: pn532::Interface,
{
    let a_record = crate::ndef::Record::new_url_record(url);
    let ndef_struct = crate::ndef::NDEFStructure::new(a_record);
    ensure_tag_formatted(pn532, timeout).await?;
    process_ntag_write_long(pn532, &ndef_struct.to_bytes().unwrap(), 4, timeout).await?;
    Ok(())
}

pub async fn read_ndef_record<I>(
    pn532: &mut pn532::Pn532<I, Esp32TimerAsync>,
    timeout: Duration,
) -> Result<Option<crate::ndef::Record>, Error<I::Error>>
where
    I: pn532::Interface,
{
    let mut page34 = [0u8; 8];
    // first get ndef first page to extract message length
    crate::pn532_ext::process_ntag_read_long(pn532, &mut page34, 3, 8, timeout).await?;

    let is_ndef_capable = page34[0] == 0xE1;
    let ndef_message_exist = is_ndef_capable && page34[4] == 0x03; // first byte of page 4
    if !ndef_message_exist {
        return Err(Error::NotNdefFormatted);
    }
    let ndef_message_empty = ndef_message_exist && page34[4 + 1] == 0x00; // second byte of page 4
    if ndef_message_empty {
        return Ok(None);
    }
    // read data for message
    let message_size_or_marker = page34[4 + 1];
    let mut buf_size: usize = if message_size_or_marker == 0xff {
        page34[4+2] as usize *256 +  page34[4+3] as usize  + 5 /* for long TLV  */
    } else {
        message_size_or_marker as usize + 3 /* for  short TLV */
    };
    buf_size =  (buf_size+3) &!3; // align to 4 bytes (for the padding in the ndef)
    let mut buf_vec = alloc::vec![0u8;buf_size];
    let buf: &mut [u8] = &mut buf_vec;
    crate::pn532_ext::process_ntag_read_long(pn532, buf, 4, buf_size, timeout)
        .await?;

    match crate::ndef::NDEFStructure::from_bytes((buf, 0)) {
        Err(e) => Err(Error::NdefReadError(e)),
        Ok(ndef) => Ok(Some(ndef.1.record)),
    }
}
