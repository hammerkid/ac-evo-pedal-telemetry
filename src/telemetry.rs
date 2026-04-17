//! AC Evo shared memory reader and background telemetry polling thread.
//!
//! Reads physics data from `acevo_pmf_physics` (AC Evo 0.6+) or the legacy
//! `acpmf_physics` mapping via Win32 FFI. Includes a probe subsystem that
//! scans all known SHM names, diffs against a baseline, and logs candidate
//! ABS/TC fields to `probe_log.txt`.

use std::io::Write;
use std::sync::mpsc;
use std::time::Instant;

// ---------------------------------------------------------------------------
// Public data types
// ---------------------------------------------------------------------------

/// Data packet sent from the telemetry thread to the UI via [`mpsc`] channel.
pub struct TelemetryData {
    pub sim_name: String,
    pub gear: Option<i8>,
    pub speed_kmh: Option<f64>,
    pub rpm: Option<f64>,
    pub max_rpm: Option<f64>,
    pub abs_active: bool,
    pub tc_active: bool,
    pub abs_vibration: f32,
    pub steer_angle: Option<f32>,
    pub pedals_game: Option<(f64, f64, f64)>,
    pub pedals_raw: Option<(f64, f64, f64)>,
    pub abs_source: &'static str,
    pub probe_values: Vec<(usize, f32)>,
}

/// Snapshot of fields read directly from AC's physics shared memory.
pub(crate) struct AcPhysicsSnapshot {
    pub gas: f32,
    pub brake: f32,
    pub clutch: f32,
    pub steer_angle: f32,
    pub speed_kmh: f32,
    pub gear: i32,
    pub rpms: i32,
    pub abs_active: bool,
    pub tc_active: bool,
    pub abs_vibration: f32,
}

// ---------------------------------------------------------------------------
// AC Shared Memory — Win32 FFI
// ---------------------------------------------------------------------------

/// Read physics fields from AC's shared memory page.
/// AC Evo 0.6+: `acevo_pmf_physics`. Falls back to classic `acpmf_physics`.
#[cfg(windows)]
pub(crate) fn read_ac_shared_physics() -> Option<AcPhysicsSnapshot> {
    extern "system" {
        fn OpenFileMappingA(access: u32, inherit: i32, name: *const u8) -> isize;
        fn MapViewOfFile(h: isize, access: u32, off_hi: u32, off_lo: u32, bytes: usize) -> *mut u8;
        fn UnmapViewOfFile(base: *const u8) -> i32;
        fn CloseHandle(handle: isize) -> i32;
    }
    const FILE_MAP_READ: u32 = 4;

    unsafe {
        let mut handle = OpenFileMappingA(
            FILE_MAP_READ, 0,
            b"Local\\acevo_pmf_physics\0".as_ptr(),
        );
        if handle == 0 {
            handle = OpenFileMappingA(
                FILE_MAP_READ, 0,
                b"Local\\acpmf_physics\0".as_ptr(),
            );
        }
        if handle == 0 { return None; }

        let ptr = MapViewOfFile(handle, FILE_MAP_READ, 0, 0, 0);
        if ptr.is_null() {
            CloseHandle(handle);
            return None;
        }

        // Standard AC physics struct (packed(4)):
        // packetId(0), gas(4), brake(8), fuel(12), gear(16,i32),
        // rpms(20,i32), steerAngle(24), speedKmh(28), ...
        let gas         = *(ptr.add(4)   as *const f32);
        let brake       = *(ptr.add(8)   as *const f32);
        let gear        = *(ptr.add(16)  as *const i32);
        let rpms        = *(ptr.add(20)  as *const i32);
        let steer_angle = *(ptr.add(24)  as *const f32);
        let speed_kmh   = *(ptr.add(28)  as *const f32);
        let clutch      = *(ptr.add(364) as *const f32);

        // TC/ABS: f32 oscillation fields (i32 flags are always 0 in AC Evo)
        let abs_field   = *(ptr.add(252) as *const f32);
        let tc_field    = *(ptr.add(204) as *const f32);

        UnmapViewOfFile(ptr);
        CloseHandle(handle);

        Some(AcPhysicsSnapshot {
            gas, brake, clutch, steer_angle, speed_kmh, gear, rpms,
            abs_active: abs_field > 0.01,
            tc_active: tc_field > 0.5,
            abs_vibration: abs_field.clamp(0.0, 1.0),
        })
    }
}

#[cfg(not(windows))]
pub(crate) fn read_ac_shared_physics() -> Option<AcPhysicsSnapshot> {
    None
}

// ---------------------------------------------------------------------------
// SHM Probe infrastructure
// ---------------------------------------------------------------------------

const SHM_NAMES: &[&[u8]] = &[
    b"Local\\acevo_pmf_physics\0",
    b"Local\\acevo_pmf_graphics\0",
    b"Local\\acevo_pmf_static\0",
    b"Local\\acpmf_physics\0",
    b"Local\\acpmf_graphics\0",
    b"Local\\acpmf_static\0",
];

#[cfg(windows)]
fn probe_shm_by_name(name: &[u8], byte_len: usize) -> Option<(bool, Vec<(usize, f32)>)> {
    extern "system" {
        fn OpenFileMappingA(access: u32, inherit: i32, name: *const u8) -> isize;
        fn MapViewOfFile(h: isize, access: u32, off_hi: u32, off_lo: u32, bytes: usize) -> *mut u8;
        fn UnmapViewOfFile(base: *const u8) -> i32;
        fn CloseHandle(handle: isize) -> i32;
    }
    const FILE_MAP_READ: u32 = 4;

    unsafe {
        let handle = OpenFileMappingA(FILE_MAP_READ, 0, name.as_ptr());
        if handle == 0 { return None; }
        let ptr = MapViewOfFile(handle, FILE_MAP_READ, 0, 0, byte_len);
        if ptr.is_null() {
            CloseHandle(handle);
            return None;
        }

        let mut values = Vec::with_capacity(byte_len / 4);
        let mut any_nonzero = false;
        for i in 0..(byte_len / 4) {
            let offset = i * 4;
            let val = *(ptr.add(offset) as *const f32);
            if val.to_bits() != 0 { any_nonzero = true; }
            values.push((offset, val));
        }

        UnmapViewOfFile(ptr);
        CloseHandle(handle);
        Some((any_nonzero, values))
    }
}

#[cfg(not(windows))]
fn probe_shm_by_name(_name: &[u8], _byte_len: usize) -> Option<(bool, Vec<(usize, f32)>)> {
    None
}

fn probe_all_shm(byte_len: usize) -> Vec<(String, bool, Vec<(usize, f32)>)> {
    let mut results = Vec::new();
    for name_bytes in SHM_NAMES {
        if let Some((has_data, vals)) = probe_shm_by_name(name_bytes, byte_len) {
            let name_str = std::str::from_utf8(name_bytes)
                .unwrap_or("?")
                .trim_end_matches('\0')
                .to_string();
            results.push((name_str, has_data, vals));
        }
    }
    results
}

fn find_name_bytes(active_name: &str) -> Option<&'static [u8]> {
    SHM_NAMES.iter()
        .find(|n| std::str::from_utf8(n).unwrap_or("").trim_end_matches('\0') == active_name)
        .copied()
}

// ---------------------------------------------------------------------------
// Background telemetry thread
// ---------------------------------------------------------------------------

/// Spawns a thread that polls AC shared memory at ~60 Hz and sends
/// [`TelemetryData`] over an [`mpsc`] channel. Includes SHM probe logging.
pub fn spawn_telemetry_thread() -> mpsc::Receiver<TelemetryData> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // Probe log writer
        let probe_file = std::fs::File::create("probe_log.txt").ok();
        let mut probe_writer = probe_file.map(std::io::BufWriter::new);
        let mut last_probe_time = Instant::now();
        let mut probe_baseline: Option<Vec<(usize, f32)>> = None;
        let probe_start = Instant::now();
        const PROBE_BYTES: usize = 1500;
        const PROBE_INTERVAL_MS: u128 = 200;

        if let Some(ref mut w) = probe_writer {
            let _ = writeln!(w, "=== AC Evo SHM Probe ===");
            let _ = writeln!(w, "Probing {} SHM names every {}ms, {} bytes each\n",
                SHM_NAMES.len(), PROBE_INTERVAL_MS, PROBE_BYTES);
        }

        let mut was_connected = false;
        let mut tick_count: u64 = 0;
        let mut found_active_shm: Option<String> = None;

        loop {
            std::thread::sleep(std::time::Duration::from_millis(16));
            tick_count += 1;

            // ── Periodic SHM probe scan ──
            if last_probe_time.elapsed().as_millis() >= PROBE_INTERVAL_MS {
                last_probe_time = Instant::now();
                let elapsed = probe_start.elapsed().as_secs_f64();
                let results = probe_all_shm(PROBE_BYTES);

                let do_name_scan = found_active_shm.is_none() || (tick_count % 1800 == 0);
                if do_name_scan && !results.is_empty() {
                    if let Some(ref mut w) = probe_writer {
                        let _ = writeln!(w, "[t={:.1}s] SHM scan — {} mappings found:", elapsed, results.len());
                        for (name, has_data, vals) in &results {
                            let nonzero = vals.iter().filter(|(_, v)| v.to_bits() != 0).count();
                            let _ = writeln!(w, "  {} — has_data:{} nonzero_f32s:{}/{}", name, has_data, nonzero, vals.len());
                        }
                        let _ = writeln!(w);
                        let _ = w.flush();
                    }
                }

                for (name, has_data, vals) in &results {
                    if *has_data {
                        if found_active_shm.as_deref() != Some(name.as_str()) {
                            found_active_shm = Some(name.clone());
                            if let Some(ref mut w) = probe_writer {
                                let _ = writeln!(w, "[t={:.1}s] >>> ACTIVE SHM FOUND: {} <<<", elapsed, name);
                                let _ = writeln!(w, "  Non-zero values:");
                                for &(off, val) in vals {
                                    if val.abs() > 0.0001 && val.is_finite() {
                                        let _ = writeln!(w, "    off {:>4}: {:>14.6}", off, val);
                                    }
                                }
                                let _ = writeln!(w);
                                let _ = w.flush();
                            }
                        }
                        if tick_count % 300 == 0 {
                            if let Some(ref mut w) = probe_writer {
                                let _ = writeln!(w, "[SNAPSHOT t={:.1}s] {}", elapsed, name);
                                for &(off, val) in vals {
                                    if val.abs() > 0.0001 && val.is_finite() {
                                        let _ = writeln!(w, "    off {:>4}: {:>14.6}", off, val);
                                    }
                                }
                                let _ = writeln!(w);
                                let _ = w.flush();
                            }
                        }
                        break;
                    }
                }

                if results.is_empty() && do_name_scan {
                    if let Some(ref mut w) = probe_writer {
                        let _ = writeln!(w, "[t={:.1}s] No SHM mappings found — game not running?", elapsed);
                        let _ = w.flush();
                    }
                }
            }

            // ── Read physics snapshot ──
            let snap = match read_ac_shared_physics() {
                Some(s) => s,
                None => {
                    was_connected = false;
                    continue;
                }
            };
            if !was_connected { was_connected = true; }

            // ── Probe: diff against baseline ──
            let mut probe_interesting = Vec::new();
            if let Some(ref active_name) = found_active_shm {
                if probe_baseline.is_none() {
                    if let Some(nb) = find_name_bytes(active_name) {
                        if let Some((_, vals)) = probe_shm_by_name(nb, PROBE_BYTES) {
                            probe_baseline = Some(vals);
                        }
                    }
                }
                if let Some(nb) = find_name_bytes(active_name) {
                    if let Some((_, all_vals)) = probe_shm_by_name(nb, PROBE_BYTES) {
                        if let Some(ref baseline) = probe_baseline {
                            let mut changed = Vec::new();
                            for (i, &(off, val)) in all_vals.iter().enumerate() {
                                if i < baseline.len() {
                                    let delta = (val - baseline[i].1).abs();
                                    if delta > 0.001 && val.is_finite() {
                                        changed.push((off, val, baseline[i].1, delta));
                                    }
                                }
                            }
                            if !changed.is_empty() && snap.brake > 0.1 {
                                if let Some(ref mut w) = probe_writer {
                                    let elapsed = probe_start.elapsed().as_secs_f64();
                                    let _ = writeln!(w, "[t={:.1}s] brake={:.3} | {} changed:",
                                        elapsed, snap.brake, changed.len());
                                    for &(off, val, base, delta) in &changed {
                                        let marker = if val >= 0.0 && val <= 1.5 && snap.brake > 0.3 { " <<<ABS?" } else { "" };
                                        let _ = writeln!(w, "  off {:>4}: {:>10.6} (was {:>10.6}, d={:.4}){}", off, val, base, delta, marker);
                                    }
                                    let _ = w.flush();
                                }
                            }
                            for &(off, val, _, _) in &changed {
                                if off <= 800 && val >= -1.5 && val <= 1.5 && val.is_finite() {
                                    probe_interesting.push((off, val));
                                }
                            }
                        }
                    }
                }
            }

            // ── Build and send TelemetryData ──
            let data = TelemetryData {
                sim_name: "AC Evo (SHM)".to_string(),
                gear: Some((snap.gear - 1) as i8), // AC: 0=R, 1=N, 2=1st → subtract 1
                speed_kmh: Some(snap.speed_kmh as f64),
                rpm: Some(snap.rpms as f64),
                max_rpm: None,
                abs_active: snap.abs_active,
                tc_active: snap.tc_active,
                abs_vibration: snap.abs_vibration,
                steer_angle: Some(snap.steer_angle),
                pedals_game: Some((snap.gas as f64, snap.brake as f64, snap.clutch as f64)),
                pedals_raw: None,
                abs_source: "shm",
                probe_values: probe_interesting,
            };
            if tx.send(data).is_err() { return; }
        }
    });
    rx
}
