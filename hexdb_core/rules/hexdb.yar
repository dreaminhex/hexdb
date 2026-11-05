// HexDB's bundled YARA rules, always compiled in when the `yara` feature is on
// (see src/scan.rs). Add your own with files.scan.yara_rules.
//
// Patterns are written as hex so this file doesn't itself look like the
// samples to antivirus software on the machines that build HexDB.

rule EICAR_Test_File
{
    meta:
        description = "The EICAR anti-virus test file (harmless; used to check that scanning works)"
        reference = "https://www.eicar.org/download-anti-malware-testfile/"
    strings:
        $eicar = { 58 35 4F 21 50 25 40 41 50 5B 34 5C 50 5A 58 35 34 28 50 5E 29 37 43 43 29 37 7D 24 45 49 43 41 52 2D 53 54 41 4E 44 41 52 44 2D 41 4E 54 49 56 49 52 55 53 2D 54 45 53 54 2D 46 49 4C 45 21 24 48 2B 48 2A }
    condition:
        $eicar in (0..128)
}
