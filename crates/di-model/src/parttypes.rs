//! Human names for common partition type GUIDs (GPT) and type bytes (MBR).

pub fn name(type_id: &str) -> Option<&'static str> {
    let t = type_id.trim().to_ascii_uppercase();
    Some(match t.as_str() {
        "C12A7328-F81F-11D2-BA4B-00A0C93EC93B" => "EFI System",
        "E3C9E316-0B5C-4DB8-817D-F92DF00215AE" => "Microsoft Reserved",
        "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7" => "Microsoft Basic Data",
        "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC" => "Windows Recovery",
        "5808C8AA-7E8F-42E0-85D2-E1E90434CFB3" => "Windows LDM metadata",
        "AF9B60A0-1431-4F62-BC68-3311714A69AD" => "Windows LDM data",
        "0FC63DAF-8483-4772-8E79-3D69D8477DE4" => "Linux filesystem",
        "0657FD6D-A4AB-43C4-84E5-0933C84B4F4F" => "Linux swap",
        "E6D6D379-F507-44C2-A23C-238F2A3DF928" => "Linux LVM",
        "A19D880F-05FC-4D3B-A006-743F0F84911E" => "Linux RAID",
        "CA7D7CCB-63ED-4C53-861C-1742536059CC" => "Linux LUKS",
        "4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709" => "Linux root (x86-64)",
        "BC13C2FF-59E6-4262-A352-B275FD6F7172" => "Linux extended boot",
        "21686148-6449-6E6F-744E-656564454649" => "BIOS boot",
        "7C3457EF-0000-11AA-AA11-00306543ECAC" => "Apple APFS",
        "48465300-0000-11AA-AA11-00306543ECAC" => "Apple HFS+",
        "426F6F74-0000-11AA-AA11-00306543ECAC" => "Apple Boot",
        "52637672-7900-11AA-AA11-00306543ECAC" => "Apple APFS Recovery",
        "6A898CC3-1DD2-11B2-99A6-080020736631" => "ZFS",
        "0X07" => "NTFS / exFAT",
        "0X0B" | "0X0C" => "FAT32",
        "0X04" | "0X06" | "0X0E" => "FAT16",
        "0X01" => "FAT12",
        "0X05" | "0X0F" | "0X85" => "Extended",
        "0X82" => "Linux swap",
        "0X83" => "Linux",
        "0X8E" => "Linux LVM",
        "0XFD" => "Linux RAID",
        "0XAF" => "Apple HFS+",
        "0XEE" => "GPT protective",
        "0XEF" => "EFI System",
        "0X27" => "Windows Recovery",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn names() {
        assert_eq!(super::name("ebd0a0a2-b9e5-4433-87c0-68b6b72699c7"), Some("Microsoft Basic Data"));
        assert_eq!(super::name("0x07"), Some("NTFS / exFAT"));
        assert_eq!(super::name("0x99"), None);
    }
}
