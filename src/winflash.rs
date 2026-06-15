use std::io::{Read, Seek, SeekFrom};

use anyhow::{bail, Context, Result};

pub fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut s = bytes as f64;
    let mut i = 0;
    while s >= 1024.0 && i < UNITS.len() - 1 {
        s /= 1024.0;
        i += 1;
    }
    format!("{:.1} {}", s, UNITS[i])
}

/// Information about a single GPT partition.
#[derive(Debug)]
pub struct GptPartition {
    pub index: u32,
    pub start_lba: u64,
    pub end_lba: u64,
    pub byte_offset: u64,
    pub byte_size: u64,
    pub partition_type_guid: [u8; 16],
    pub name: String,
}

/// Parse a raw disk image or physical drive to find GPT partitions.
pub fn read_gpt_partitions<T: Read + Seek>(file: &mut T) -> Result<Vec<GptPartition>> {
    const LBA_SIZE: u64 = 512;

    // Read GPT header at LBA 1
    let mut header_buf = vec![0u8; 92];
    file.seek(SeekFrom::Start(LBA_SIZE))?;
    file.read_exact(&mut header_buf)?;

    let signature = &header_buf[0..8];
    if signature != b"EFI PART" {
        bail!("Not a valid GPT disk (missing EFI PART signature)");
    }

    let partition_entry_lba = u64::from_le_bytes(
        header_buf[72..80].try_into().unwrap(),
    );
    let num_entries = u32::from_le_bytes(
        header_buf[80..84].try_into().unwrap(),
    );
    let entry_size = u32::from_le_bytes(
        header_buf[84..88].try_into().unwrap(),
    );

    if entry_size < 128 || num_entries == 0 {
        bail!("Invalid GPT partition entry format");
    }

    // Read partition entries
    let entries_byte_offset = partition_entry_lba * LBA_SIZE;
    let entries_total_size = num_entries as u64 * entry_size as u64;
    let mut entries_buf = vec![0u8; entries_total_size as usize];
    file.seek(SeekFrom::Start(entries_byte_offset))?;
    file.read_exact(&mut entries_buf)?;

    let mut partitions = Vec::new();
    for i in 0..num_entries as usize {
        let offset = i * entry_size as usize;
        let entry = &entries_buf[offset..offset + entry_size as usize];

        let type_guid = &entry[0..16];
        if type_guid.iter().all(|&b| b == 0) {
            continue; // unused entry
        }

        let start_lba = u64::from_le_bytes(
            entry[32..40].try_into().unwrap(),
        );
        let end_lba = u64::from_le_bytes(
            entry[40..48].try_into().unwrap(),
        );

        // Read UTF-16LE partition name (max 36 characters = 72 bytes at offset 56)
        let name_bytes = &entry[56..56 + 72];
        let name_utf16: Vec<u16> = name_bytes
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&c| c != 0)
            .collect();
        let name = String::from_utf16_lossy(&name_utf16);

        let mut type_guid_arr = [0u8; 16];
        type_guid_arr.copy_from_slice(type_guid);

        partitions.push(GptPartition {
            index: i as u32 + 1,
            start_lba,
            end_lba,
            byte_offset: start_lba * LBA_SIZE,
            byte_size: (end_lba - start_lba + 1) * LBA_SIZE,
            partition_type_guid: type_guid_arr,
            name,
        });
    }

    Ok(partitions)
}

/// Parse GPT partitions from a file path.
pub fn read_gpt_partitions_from_path(path: &str) -> Result<Vec<GptPartition>> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("Failed to open {}", path))?;
    read_gpt_partitions(&mut file)
}

#[cfg(target_os = "windows")]
mod win {
    use std::io::{Read, Write};

    use anyhow::{bail, Context, Result};

    /// Open a physical drive on Windows and return a file-like handle.
    pub fn open_physical_drive(drive_index: u32) -> Result<std::fs::File> {
        let path = format!(r"\\.\PhysicalDrive{}", drive_index);
        // Use OpenOptions with explicit sharing flags for physical drive access
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 1;
        const FILE_SHARE_WRITE: u32 = 2;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&path)
            .with_context(|| format!("Failed to open {}", path))?;
        Ok(file)
    }

    /// List Windows physical drives via PowerShell.
    pub fn list_physical_drives() -> Result<Vec<(u32, String, u64)>> {
        let output = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", r#"
Get-CimInstance -ClassName Win32_DiskDrive | Select-Object Index, Model, Size | ConvertTo-Json
"#])
            .output()
            .context("Failed to enumerate physical drives")?;

        if !output.status.success() {
            bail!("PowerShell enumeration failed");
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let trimmed = stdout.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }

        #[derive(serde::Deserialize)]
        struct RawDrive {
            #[serde(alias = "Index")]
            index: u32,
            #[serde(alias = "Model")]
            model: Option<String>,
            #[serde(alias = "Size")]
            size: Option<u64>,
        }

        let raw: Vec<RawDrive> = if trimmed.starts_with('{') {
            serde_json::from_str(trimmed).map(|d| vec![d]).unwrap_or_default()
        } else {
            serde_json::from_str(trimmed).unwrap_or_default()
        };

        Ok(raw
            .into_iter()
            .map(|d| (d.index, d.model.unwrap_or_default(), d.size.unwrap_or(0)))
            .collect())
    }

    const CHUNK_SIZE: u64 = 8 * 1024 * 1024; // 8 MiB

    /// Clean the disk via diskpart, removing all partitions and releasing volume locks.
    fn clean_disk_via_diskpart(drive_index: u32) -> Result<()> {
        let script = format!("select disk {}\r\nclean\r\n", drive_index);
        let mut child = std::process::Command::new("diskpart")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("Failed to start diskpart")?;

        use std::io::Write;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(script.as_bytes())
                .context("Failed to write diskpart script")?;
        }

        let status = child.wait().context("diskpart did not exit")?;
        if !status.success() {
            let stderr = if let Some(mut stderr) = child.stderr.take() {
                let mut buf = String::new();
                use std::io::Read;
                stderr.read_to_string(&mut buf).ok();
                buf
            } else {
                String::new()
            };
            bail!("diskpart clean failed: {}", stderr.trim());
        }
        Ok(())
    }

    /// Stream the full image (MBR + GPT header + partitions) to the physical drive in chunks.
    /// Writes from byte 0 up to the end of the last partition, so the target gets a valid GPT.
    pub fn flash_image_to_drive(image_path: &str, drive_index: u32) -> Result<()> {
        // Parse image partitions to know the write extent
        let img_parts = {
            let mut f =
                std::fs::File::open(image_path).context("Failed to open image file")?;
            super::read_gpt_partitions(&mut f)?
        };
        if img_parts.is_empty() {
            bail!("Image has no GPT partitions");
        }

        // Write from LBA 0 to the end of the last partition
        let last = img_parts.last().unwrap();
        let write_end = last.byte_offset + last.byte_size;

        let human = |b: u64| -> String {
            const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
            let mut s = b as f64;
            let mut i = 0;
            while s >= 1024.0 && i < UNITS.len() - 1 {
                s /= 1024.0;
                i += 1;
            }
            format!("{:.1} {}", s, UNITS[i])
        };

        println!("    image partitions: {}", img_parts.iter().map(|p| format!("#{} {} @ {} size {}", p.index, p.name, human(p.byte_offset), human(p.byte_size))).collect::<Vec<_>>().join(", "));
        println!("    total write extent: {} bytes ({})", write_end, human(write_end));

        // Clean disk via diskpart to release volume locks
        println!("    cleaning disk via diskpart...");
        clean_disk_via_diskpart(drive_index)?;

        // Open drive with proper sharing flags
        let mut drive = open_physical_drive(drive_index)?;

        // Stream the image in chunks from byte 0 to write_end
        let mut img_file =
            std::fs::File::open(image_path).context("Failed to re-open image file")?;
        let mut buf = vec![0u8; CHUNK_SIZE as usize];
        let mut remaining = write_end;

        println!("    imaging drive...");
        while remaining > 0 {
            let to_read = CHUNK_SIZE.min(remaining);
            let buf_slice = &mut buf[..to_read as usize];
            img_file
                .read_exact(buf_slice)
                .context("Failed to read image chunk")?;
            drive
                .write_all(buf_slice)
                .context("Failed to write drive chunk")?;
            remaining -= to_read;
        }
        drive.flush().context("Failed to flush drive")?;
        println!("    wrote {} bytes to drive", write_end);

        Ok(())
    }
}

#[cfg(target_os = "windows")]
pub use win::*;
