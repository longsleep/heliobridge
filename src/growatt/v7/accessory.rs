//! **Accessory telemetry**: the device reporting what an accessory it polls is reading, as JSON.
//!
//! Once an accessory is paired the datalogger polls it and then tells the server what it read, once a
//! minute, in a frame of its own — address `0x6f`, function `0x64`. This module decodes it.
//!
//! # The name is ours, and the schema is one schema
//!
//! The vendor does not name this document anywhere in the firmware, so *accessory telemetry* is this
//! project's word for it. What the firmware does carry is the **complete key list, contiguous in one
//! block** — 80 keys, which is how we know it is a single schema rather than one per manufacturer:
//!
//! ```text
//! identity     manu model sw_ver prod_date cert_mark
//! bus          rs485_addr pt ct
//! status       access comm_sts op_sts alarm last_upd
//! per phase    {a,b,c}_ cur volt act aprt te re valid uca ae reae rea
//! totals       t_ cur act aprt pf avg ae ae_pos ae_neg reae reae_pos reae_neg
//!              t_ net anet anet_pos anet_neg rnet rnet_pos rnet_neg net_total dmd freq rea
//! environment  env_ ws wd temp mt hum lig rad rain snow gas press
//! ```
//!
//! `rs485_addr`, `pt` and `ct` belong to a wired Modbus meter and `env_*` to a weather sensor — wind
//! speed, humidity, rainfall — so **this is not a meter document at all**. It is one superset for every
//! third-party accessory the datalogger can poll, each populating the fields it has. That also settles
//! the vendor question: the per-model knowledge sits on the *input* side, where the firmware carries a
//! parser per meter family, and every one of them is re-emitted through this.
//!
//! Only the fields with a use are typed below. The rest are recorded in `FINDINGS.md` rather than
//! written out as eighty optional floats nothing reads.
//!
//! # It is a report, and only a report
//!
//! The direction is settled: 3 of these frames up and none down, across the whole capture set. Publishing
//! one *to* a device does nothing — 59 were tried across nine sender identities and moved no register — so
//! nothing here builds a frame, only reads one. A server that wants to supply a reading writes the meter
//! registers instead ([`crate::growatt::v7::meter`]).
//!
//! # Why decode it at all
//!
//! Two reasons, neither of them "a server needs it". The per-phase detail appears nowhere else: the
//! inverter is told a single total through registers 309–312, so voltage, current, power factor and
//! frequency per phase exist only here. And it is the only frame that names the accessory, so it is what
//! says *which* accessory a reading came from.
//!
//! The third reason is smaller and immediate: undecoded, it arrives once a minute as a warning with a hex
//! dump, which is a lot of log for a frame we understand.
//!
//! # The layout
//!
//! Offsets into the body, i.e. past the 8-octet header:
//!
//! ```text
//!  0..30   device id, as every frame
//! 30..60   the accessory's own serial, ASCII, NUL-padded
//! 60..67   timestamp: year-2000, month, day, hour, minute, second, milliseconds
//! 67..71   length of the JSON, big-endian
//! 71..     the JSON itself
//! ```
//!
//! Taken from the `parse_noah_6f64` routine in GroBro and **verified against a captured frame**: the declared length was
//! 439 and exactly 439 octets followed, and the timestamp decoded to the second the frame arrived. Note the
//! timestamp is **seven** octets here, one more than telemetry's, the extra one being milliseconds.
//!
//! The accessory serial is the decimal MAC the device reported during discovery, as ASCII here rather than
//! as the integer register 123 carries.
//!
//! # The device normalises; it does not forward
//!
//! The field names are the datalogger's own, not the meter's. The simulated Shelly this was captured
//! against served `total_act_power` and `a_act_power` inside a generation-2 document of `em:0`, `emdata:0`,
//! `sys` and `wifi`; what comes out here is `t_act` and `a_act`, plus `manu`, `model`, `comm_sts`, `op_sts`,
//! `alarm` and `err`, none of which the meter's API returned. The per-model knowledge sits on the *input*
//! side — the firmware carries a parser per meter family — so one schema for every vendor is the reasonable
//! reading. It is still an inference: one meter has been observed.

use serde::Deserialize;
use snafu::{OptionExt as _, ResultExt as _, Snafu};

/// Where the JSON's declared length sits in the body.
const LENGTH_AT: usize = 67;

/// Where the JSON itself begins.
const JSON_AT: usize = 71;

/// Why an accessory-telemetry document could not be read.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum AccessoryTelemetryError {
    /// No JSON document in the body.
    #[snafu(display("no JSON object in the body"))]
    NoJson,

    /// The JSON did not parse, or did not have the shape of this schema.
    #[snafu(display("the accessory JSON did not parse"))]
    Json {
        /// What serde said.
        source: serde_json::Error,
    },
}

/// One accessory-telemetry document.
///
/// Every field defaults: the schema is a superset covering meters, wired Modbus meters and weather
/// sensors, so most keys are absent from any given accessory's report and an absence is not an error.
/// Names are the vendor's own, abbreviated as they appear on the wire — `t_act` is the total active
/// power, `a_cur` the current on phase A.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AccessoryTelemetry {
    /// Manufacturer, as the datalogger's own accessory table spells it — `shelly`, not `Shelly`.
    pub manu: String,
    /// Model code, again the table's spelling: `SPEM-003CEBEU` for a Shelly Pro 3EM.
    pub model: String,
    /// The accessory's serial: the decimal MAC discovered during pairing.
    pub sn: String,
    /// What the accessory was enrolled with, echoed back — the same field the pair command carries.
    ///
    /// `0` means the datalogger **uses** what it reads. Enrol without the field and it becomes `1`, with
    /// which the accessory is polled, answers, and appears here with `comm_sts` 1 and correct figures while
    /// the device's own meter registers stay zero. The accessory list cannot distinguish the two, so this
    /// is the only read-back there is.
    #[serde(default)]
    pub access: i64,
    /// Whether the datalogger is managing to talk to the accessory. `1` while polling succeeds.
    #[serde(default)]
    pub comm_sts: i64,
    /// Whether the accessory reports itself operational. `1` observed.
    #[serde(default)]
    pub op_sts: i64,
    /// The accessory's own alarm word. `0` observed.
    #[serde(default)]
    pub alarm: i64,
    /// The accessory's own error word. `0` observed.
    #[serde(default)]
    pub err: i64,
    /// Total active power, watts. The figure that reaches the inverter.
    #[serde(default)]
    pub t_act: f64,
    /// Total apparent power.
    #[serde(default)]
    pub t_aprt: f64,
    /// Total current.
    #[serde(default)]
    pub t_cur: f64,
    /// Cumulative active energy imported.
    #[serde(default)]
    pub t_ae_pos: f64,
    /// Cumulative active energy exported.
    #[serde(default)]
    pub t_ae_neg: f64,
    /// Active power on phase A.
    #[serde(default)]
    pub a_act: f64,
    /// Active power on phase B.
    #[serde(default)]
    pub b_act: f64,
    /// Active power on phase C.
    #[serde(default)]
    pub c_act: f64,
    /// Voltage on phase A.
    #[serde(default)]
    pub a_volt: f64,
}

impl AccessoryTelemetry {
    /// Read one document out of a decoded frame body.
    ///
    /// The declared length is used where it agrees with what is present and ignored where it does not: a
    /// frame whose length field disagrees with its payload is still worth decoding, and the closing brace
    /// is a second opinion that costs nothing.
    ///
    /// # Errors
    ///
    /// [`AccessoryTelemetryError`] if there is no JSON in the body or it does not fit this schema.
    pub fn parse(body: &[u8]) -> Result<Self, AccessoryTelemetryError> {
        let json = Self::declared(body)
            .or_else(|| Self::found(body))
            .context(NoJsonSnafu)?;
        serde_json::from_slice(json).context(JsonSnafu)
    }

    /// The JSON the length field points at, when the body is long enough to hold it.
    fn declared(body: &[u8]) -> Option<&[u8]> {
        let field = body.get(LENGTH_AT..LENGTH_AT.checked_add(4)?)?;
        let len = usize::try_from(u32::from_be_bytes(<[u8; 4]>::try_from(field).ok()?)).ok()?;
        let document = body.get(JSON_AT..JSON_AT.checked_add(len)?)?;
        document.first()?.eq(&b'{').then_some(document)
    }

    /// The JSON located by its braces, for a frame whose length field cannot be trusted.
    fn found(body: &[u8]) -> Option<&[u8]> {
        let start = body.iter().position(|&b| b == b'{')?;
        let document = body.get(start..)?;
        let end = document.iter().rposition(|&b| b == b'}')?.checked_add(1)?;
        document.get(..end)
    }

    /// Whether the datalogger is currently managing to read the accessory.
    pub const fn communicating(&self) -> bool {
        self.comm_sts != 0
    }

    /// Whether the accessory is reporting a fault of either kind.
    pub const fn faulted(&self) -> bool {
        self.alarm != 0 || self.err != 0
    }
}

#[cfg(test)]
mod tests {
    use super::AccessoryTelemetry;

    /// The body of a real frame, from `captures/accessory-enrolment-2026-09-06.txt`, with the device's own
    /// serial replaced. The accessory serial is the decimal MAC of the simulated meter.
    fn body() -> Vec<u8> {
        let mut body = b"0EXAMPLE00000001".to_vec();
        body.extend(std::iter::repeat_n(0u8, 14));
        body.extend(b"187723572702975");
        body.extend(std::iter::repeat_n(0u8, 15));
        body.extend([0x1a, 0x09, 0x06, 0x11, 0x27, 0x02, 0x01, 0x00, 0x00, 0x01, 0xb7]);
        body.extend(
            br#"{"manu":"shelly","model":"SPEM-003CEBEU","sn":"187723572702975","access":0,"comm_sts":1,"op_sts":1,"alarm":0,"a_cur":0.522,"a_volt":230,"a_act":120,"a_aprt":120,"a_pf":1,"a_freq":50,"a_te":0,"a_re":0,"b_cur":0,"b_volt":230,"b_act":0,"b_aprt":0,"b_pf":0,"b_freq":50,"b_te":0,"b_re":0,"c_cur":0,"c_volt":230,"c_act":0,"c_aprt":0,"c_pf":0,"c_freq":50,"c_te":0,"c_re":0,"t_cur":0.522,"t_act":120,"t_aprt":120,"t_ae_pos":0,"t_ae_neg":0,"err":0}"#,
        );
        body.push(0);
        body
    }

    #[test]
    fn a_real_report_decodes() {
        let report = AccessoryTelemetry::parse(&body()).expect("the captured frame decodes");
        assert_eq!(report.manu, "shelly");
        assert_eq!(report.model, "SPEM-003CEBEU");
        assert_eq!(report.sn, "187723572702975");
        assert!((report.t_act - 120.0).abs() < f64::EPSILON);
        assert!((report.a_act - 120.0).abs() < f64::EPSILON);
        assert!(report.b_act.abs() < f64::EPSILON);
        assert!(report.communicating());
        assert!(!report.faulted());
    }

    #[test]
    fn the_json_is_found_rather_than_addressed() {
        // The fields ahead of the document have no length field, so the parse must not depend on the
        // offset the observed frames happen to use.
        let mut shifted = b"0EXAMPLE00000001".to_vec();
        shifted.extend(std::iter::repeat_n(0u8, 40));
        shifted.extend(br#"{"manu":"shelly","model":"X","sn":"1","t_act":-42.5}"#);
        let report = AccessoryTelemetry::parse(&shifted).expect("still decodes");
        assert!((report.t_act + 42.5).abs() < f64::EPSILON);
        assert_eq!(report.sn, "1");
    }

    #[test]
    fn a_body_without_json_is_an_error_rather_than_a_panic() {
        assert!(AccessoryTelemetry::parse(b"").is_err());
        assert!(AccessoryTelemetry::parse(&[0u8; 70]).is_err());
        assert!(AccessoryTelemetry::parse(b"0EXAMPLE00000001 no document here").is_err());
    }

    #[test]
    fn a_truncated_document_is_an_error() {
        let mut cut = b"0EXAMPLE00000001".to_vec();
        cut.extend(std::iter::repeat_n(0u8, 40));
        cut.extend(br#"{"manu":"shelly","model":"X""#);
        assert!(AccessoryTelemetry::parse(&cut).is_err());
    }

    #[test]
    fn a_fault_is_either_word() {
        let mut faulted = b"0EXAMPLE00000001".to_vec();
        faulted.extend(std::iter::repeat_n(0u8, 40));
        faulted.extend(br#"{"manu":"m","model":"x","sn":"1","alarm":0,"err":3}"#);
        let report = AccessoryTelemetry::parse(&faulted).expect("decodes");
        assert!(report.faulted());
        assert!(!report.communicating(), "comm_sts defaults to 0 when absent");
    }
}
