//! Identity of the driver: CLSID, names and registry paths.

use asio_abi::Guid;

/// CLSID of "EQ ASIO Router" (also the IID hosts use to QueryInterface for IASIO).
/// `{D9568DC8-2E04-429B-ABD0-03852E2931DF}`
pub const CLSID_EQ_ASIO: Guid = Guid {
    data1: 0xD9568DC8,
    data2: 0x2E04,
    data3: 0x429B,
    data4: [0xAB, 0xD0, 0x03, 0x85, 0x2E, 0x29, 0x31, 0xDF],
};

pub const IID_IUNKNOWN: Guid = Guid { data1: 0, data2: 0, data3: 0, data4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46] };

pub const IID_ICLASSFACTORY: Guid = Guid { data1: 1, data2: 0, data3: 0, data4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46] };

pub const DRIVER_NAME: &str = "EQ ASIO Router";
pub const DRIVER_DESCRIPTION: &str = "EQ ASIO Router (virtual ASIO driver with AutoEQ)";
pub const DRIVER_VERSION: i32 = 1;
pub const CONTROL_PANEL_EXE: &str = "eq-asio-app.exe";

/// Output channels we expose (the Bridge has 8 route buttons); 0 and 1 are the EQ pair.
pub const NUM_OUTPUTS: i32 = 8;

pub fn guid_to_string(g: &Guid) -> String {
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        g.data1,
        g.data2,
        g.data3,
        g.data4[0],
        g.data4[1],
        g.data4[2],
        g.data4[3],
        g.data4[4],
        g.data4[5],
        g.data4[6],
        g.data4[7]
    )
}

pub fn clsid_string() -> String {
    guid_to_string(&CLSID_EQ_ASIO)
}

pub fn clsid_key_path() -> String {
    format!("SOFTWARE\\Classes\\CLSID\\{}", clsid_string())
}

pub fn inproc_key_path() -> String {
    format!("{}\\InprocServer32", clsid_key_path())
}

pub fn asio_key_path() -> String {
    format!("SOFTWARE\\ASIO\\{DRIVER_NAME}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clsid_string_format() {
        assert_eq!(clsid_string(), "{D9568DC8-2E04-429B-ABD0-03852E2931DF}");
        assert_eq!(guid_to_string(&IID_IUNKNOWN), "{00000000-0000-0000-C000-000000000046}");
        assert_eq!(guid_to_string(&IID_ICLASSFACTORY), "{00000001-0000-0000-C000-000000000046}");
    }

    #[test]
    fn registry_paths() {
        assert_eq!(clsid_key_path(), "SOFTWARE\\Classes\\CLSID\\{D9568DC8-2E04-429B-ABD0-03852E2931DF}");
        assert!(inproc_key_path().ends_with("}\\InprocServer32"));
        assert_eq!(asio_key_path(), "SOFTWARE\\ASIO\\EQ ASIO Router");
    }
}
