//! ATA CHECK POWER MODE over SG_IO: asks a disk whether it is spun up,
//! without waking it.
//!
//! The command goes through the SCSI layer as ATA PASS-THROUGH (16) with
//! `CK_COND` set, so that the disk's ATA registers come back in the sense
//! data; the count register holds the power mode. Sending it needs
//! `CAP_SYS_RAWIO`. The kernel does not count pass-through commands as the
//! disk's I/O, so asking does not disturb the I/O counters either.

use std::os::fd::AsRawFd;
use std::path::Path;

/// `SG_IO` from `<scsi/sg.h>`; libc does not export it.
const SG_IO: u32 = 0x2285;
/// `SG_DXFER_NONE`: the command transfers no data.
const SG_DXFER_NONE: i32 = -1;
/// The opcode of ATA PASS-THROUGH (16).
const ATA_16: u8 = 0x85;
/// The opcode of ATA CHECK POWER MODE.
const CHECK_POWER_MODE: u8 = 0xe5;
/// How long the disk may take to answer, in milliseconds.
const TIMEOUT_MS: u32 = 5_000;
/// Room for the sense data.
const SENSE_LEN: usize = 32;

/// What a disk is doing, as its answer to CHECK POWER MODE tells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerMode {
    /// In standby: spun down.
    Standby,
    /// Active or idle, holding the given power mode byte: spun up.
    SpunUp(u8),
}

/// What came of asking a disk for its power mode.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    PowerMode(PowerMode),
    /// The disk did not answer, as one in the sleep state does not.
    Silent(String),
    /// The path to the disk cannot pass the command on, or the disk does not
    /// know it: its power mode must be judged some other way.
    Unsupported(String),
}

/// The command descriptor block: ATA PASS-THROUGH (16), non-data protocol,
/// `CK_COND` set, carrying CHECK POWER MODE.
pub fn cdb() -> [u8; 16] {
    let mut cdb = [0; 16];
    cdb[0] = ATA_16;
    // PROTOCOL 3 (non-data), not extended.
    cdb[1] = 3 << 1;
    // CK_COND: return the ATA registers; no data transferred.
    cdb[2] = 0x20;
    cdb[14] = CHECK_POWER_MODE;
    cdb
}

/// `sg_io_hdr` from `<scsi/sg.h>`.
#[repr(C)]
struct SgIoHdr {
    interface_id: libc::c_int,
    dxfer_direction: libc::c_int,
    cmd_len: libc::c_uchar,
    mx_sb_len: libc::c_uchar,
    iovec_count: libc::c_ushort,
    dxfer_len: libc::c_uint,
    dxferp: *mut libc::c_void,
    cmdp: *const libc::c_uchar,
    sbp: *mut libc::c_uchar,
    timeout: libc::c_uint,
    flags: libc::c_uint,
    pack_id: libc::c_int,
    usr_ptr: *mut libc::c_void,
    status: libc::c_uchar,
    masked_status: libc::c_uchar,
    msg_status: libc::c_uchar,
    sb_len_wr: libc::c_uchar,
    host_status: libc::c_ushort,
    driver_status: libc::c_ushort,
    resid: libc::c_int,
    duration: libc::c_uint,
    info: libc::c_uint,
}

/// Asks the disk at `dev`, such as `/dev/sdb`, for its power mode.
pub fn check_power_mode(dev: &Path) -> Answer {
    use std::os::unix::fs::OpenOptionsExt;

    // As hdparm opens it: read-only and non-blocking, which wakes nothing.
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(dev)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Answer::Unsupported(format!("cannot open {}: {e}", dev.display()));
        }
        Err(e) => return Answer::Silent(format!("cannot open {}: {e}", dev.display())),
    };
    let cdb = cdb();
    let mut sense = [0u8; SENSE_LEN];
    let mut hdr = SgIoHdr {
        interface_id: b'S' as libc::c_int,
        dxfer_direction: SG_DXFER_NONE,
        cmd_len: cdb.len() as libc::c_uchar,
        mx_sb_len: SENSE_LEN as libc::c_uchar,
        iovec_count: 0,
        dxfer_len: 0,
        dxferp: std::ptr::null_mut(),
        cmdp: cdb.as_ptr(),
        sbp: sense.as_mut_ptr(),
        timeout: TIMEOUT_MS,
        flags: 0,
        pack_id: 0,
        usr_ptr: std::ptr::null_mut(),
        status: 0,
        masked_status: 0,
        msg_status: 0,
        sb_len_wr: 0,
        host_status: 0,
        driver_status: 0,
        resid: 0,
        duration: 0,
        info: 0,
    };
    // SAFETY: `hdr` points at `cdb` and `sense`, which outlive the call, and
    // gives their lengths; the command transfers no data.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), SG_IO as _, &mut hdr as *mut SgIoHdr) };
    if rc < 0 {
        let e = std::io::Error::last_os_error();
        return match e.raw_os_error() {
            Some(libc::EPERM | libc::EACCES) => {
                Answer::Unsupported(format!("SG_IO needs CAP_SYS_RAWIO: {e}"))
            }
            Some(libc::ENOTTY | libc::EINVAL | libc::EOPNOTSUPP) => {
                Answer::Unsupported(format!("{} takes no SCSI commands: {e}", dev.display()))
            }
            _ => Answer::Silent(format!("SG_IO failed: {e}")),
        };
    }
    let sense_len = (hdr.sb_len_wr as usize).min(SENSE_LEN);
    decode(
        hdr.status,
        hdr.host_status,
        hdr.driver_status,
        &sense[..sense_len],
    )
}

/// The SCSI sense key ILLEGAL REQUEST.
const ILLEGAL_REQUEST: u8 = 0x05;
/// The ATA status bit ERR.
const ATA_ERR: u8 = 0x01;
/// The ATA error bit ABRT: the device aborted the command.
const ATA_ABRT: u8 = 0x04;
/// `DRIVER_TIMEOUT`, in the low bits of the driver status.
const DRIVER_TIMEOUT: u16 = 0x06;

/// The ATA registers that came back.
struct Registers {
    error: u8,
    count: u8,
    status: u8,
}

/// Reads the answer to [`cdb`] from the SCSI status, the host and driver
/// status and the sense data that SG_IO returned.
pub fn decode(status: u8, host_status: u16, driver_status: u16, sense: &[u8]) -> Answer {
    if host_status != 0 {
        return Answer::Silent(format!("host status {host_status:#x}"));
    }
    if driver_status & 0x0f == DRIVER_TIMEOUT {
        return Answer::Silent("the command timed out".to_owned());
    }
    if sense.is_empty() {
        return if status == 0 {
            Answer::Unsupported("the answer holds no ATA registers".to_owned())
        } else {
            Answer::Silent(format!("SCSI status {status:#x} without sense data"))
        };
    }
    let (sense_key, registers) = match sense[0] & 0x7f {
        0x72 | 0x73 => (sense.get(1).map(|k| k & 0x0f), descriptor_registers(sense)),
        0x70 | 0x71 => (sense.get(2).map(|k| k & 0x0f), fixed_registers(sense)),
        code => {
            return Answer::Unsupported(format!("unknown sense data format {code:#x}"));
        }
    };
    if sense_key == Some(ILLEGAL_REQUEST) {
        return Answer::Unsupported("the device refused ATA PASS-THROUGH".to_owned());
    }
    let Some(registers) = registers else {
        return Answer::Unsupported(format!(
            "the answer holds no ATA registers (sense key {:#x})",
            sense_key.unwrap_or(0)
        ));
    };
    if registers.status & ATA_ERR != 0 {
        return if registers.error & ATA_ABRT != 0 {
            Answer::Unsupported("the disk aborted CHECK POWER MODE".to_owned())
        } else {
            Answer::Silent(format!("ATA error {:#x}", registers.error))
        };
    }
    Answer::PowerMode(match registers.count {
        // Standby_z and standby_y.
        0x00 | 0x01 => PowerMode::Standby,
        mode => PowerMode::SpunUp(mode),
    })
}

/// The registers of the ATA Status Return descriptor in descriptor-format
/// sense data, if it holds one.
fn descriptor_registers(sense: &[u8]) -> Option<Registers> {
    let end = sense.len().min(8 + *sense.get(7)? as usize);
    let mut at = 8;
    while at + 2 <= end {
        let (code, len) = (sense[at], sense[at + 1] as usize);
        let descriptor = sense.get(at..(at + 2 + len).min(end))?;
        if code == 0x09 && descriptor.len() >= 14 {
            return Some(Registers {
                error: descriptor[3],
                count: descriptor[5],
                status: descriptor[13],
            });
        }
        at += 2 + len;
    }
    None
}

/// The ATA registers in fixed-format sense data, if the additional sense
/// code says that it holds them ("ATA pass through information available").
fn fixed_registers(sense: &[u8]) -> Option<Registers> {
    if sense.get(12..14)? != [0x00, 0x1d] {
        return None;
    }
    Some(Registers {
        error: sense[3],
        status: sense[4],
        count: sense[6],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Descriptor-format sense data, as libata returns for `CK_COND`: sense
    /// key RECOVERED ERROR, "ATA pass through information available", and
    /// an ATA Status Return descriptor.
    fn descriptor_sense(error: u8, count: u8, status: u8) -> Vec<u8> {
        let mut sense = vec![0x72, 0x01, 0x00, 0x1d, 0, 0, 0, 14];
        sense.extend([
            0x09, 12, 0x00, error, 0, count, 0, 0, 0, 0, 0, 0, 0x40, status,
        ]);
        sense
    }

    /// Fixed-format sense data with the ATA registers in its information
    /// field, as some SAT bridges return.
    fn fixed_sense(error: u8, count: u8, status: u8) -> Vec<u8> {
        let mut sense = vec![0; 18];
        sense[0] = 0x70;
        sense[2] = 0x01;
        sense[3] = error;
        sense[4] = status;
        sense[6] = count;
        sense[7] = 10;
        sense[12] = 0x00;
        sense[13] = 0x1d;
        sense
    }

    /// `DRIVER_SENSE`: sense data came back, as it does for `CK_COND`.
    const DRIVER_SENSE: u16 = 0x08;
    /// SCSI status CHECK CONDITION.
    const CHECK_CONDITION: u8 = 0x02;

    #[test]
    fn the_cdb_is_check_power_mode_in_ata_pass_through_16() {
        assert_eq!(
            cdb(),
            [0x85, 0x06, 0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xe5, 0]
        );
    }

    #[test]
    fn the_count_register_tells_standby_from_spun_up() {
        for (count, mode) in [
            (0x00, PowerMode::Standby),
            (0x01, PowerMode::Standby),
            (0x80, PowerMode::SpunUp(0x80)),
            (0x81, PowerMode::SpunUp(0x81)),
            (0xff, PowerMode::SpunUp(0xff)),
            (0x40, PowerMode::SpunUp(0x40)),
        ] {
            let sense = descriptor_sense(0, count, 0x50);
            assert_eq!(
                decode(CHECK_CONDITION, 0, DRIVER_SENSE, &sense),
                Answer::PowerMode(mode),
                "count {count:#x}"
            );
            let sense = fixed_sense(0, count, 0x50);
            assert_eq!(
                decode(CHECK_CONDITION, 0, DRIVER_SENSE, &sense),
                Answer::PowerMode(mode),
                "fixed format, count {count:#x}"
            );
        }
    }

    #[test]
    fn the_status_descriptor_is_found_after_other_descriptors() {
        let mut sense = vec![0x72, 0x01, 0x00, 0x1d, 0, 0, 0, 26];
        // An information descriptor first.
        sense.extend([0x00, 10, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        sense.extend([0x09, 12, 0, 0, 0, 0xff, 0, 0, 0, 0, 0, 0, 0x40, 0x50]);

        assert_eq!(
            decode(CHECK_CONDITION, 0, DRIVER_SENSE, &sense),
            Answer::PowerMode(PowerMode::SpunUp(0xff))
        );
    }

    #[test]
    fn a_disk_that_does_not_answer_is_silent() {
        // DID_TIME_OUT, as a disk in the sleep state gives.
        assert!(matches!(decode(0, 0x03, 0, &[]), Answer::Silent(_)));
        assert!(matches!(
            decode(0, 0, DRIVER_TIMEOUT, &[]),
            Answer::Silent(_)
        ));
        // ERR without ABRT.
        let sense = descriptor_sense(0x10, 0, 0x51);
        assert!(matches!(
            decode(CHECK_CONDITION, 0, DRIVER_SENSE, &sense),
            Answer::Silent(_)
        ));
    }

    #[test]
    fn a_path_that_cannot_pass_the_command_on_is_unsupported() {
        // ILLEGAL REQUEST, INVALID COMMAND OPERATION CODE.
        let mut sense = vec![0; 18];
        sense[0] = 0x70;
        sense[2] = ILLEGAL_REQUEST;
        sense[12] = 0x20;
        assert!(matches!(
            decode(CHECK_CONDITION, 0, DRIVER_SENSE, &sense),
            Answer::Unsupported(_)
        ));
        // The disk aborted the command.
        let sense = descriptor_sense(ATA_ABRT, 0, 0x51);
        assert!(matches!(
            decode(CHECK_CONDITION, 0, DRIVER_SENSE, &sense),
            Answer::Unsupported(_)
        ));
        // GOOD status, but no registers to read the power mode from.
        assert!(matches!(decode(0, 0, 0, &[]), Answer::Unsupported(_)));
        // Fixed-format sense data about something else.
        let mut sense = fixed_sense(0, 0xff, 0x50);
        sense[13] = 0x00;
        assert!(matches!(
            decode(CHECK_CONDITION, 0, DRIVER_SENSE, &sense),
            Answer::Unsupported(_)
        ));
    }

    #[test]
    fn truncated_sense_data_is_unsupported_rather_than_a_panic() {
        let sense = descriptor_sense(0, 0xff, 0x50);
        for len in 1..sense.len() {
            let answer = decode(CHECK_CONDITION, 0, DRIVER_SENSE, &sense[..len]);
            assert!(
                matches!(answer, Answer::Unsupported(_)),
                "{len}: {answer:?}"
            );
        }
    }
}
