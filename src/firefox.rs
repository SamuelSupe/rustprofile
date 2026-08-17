use std::{
    collections::{HashMap, HashSet},
    io::BufWriter,
    path::Path,
    time::{Duration, UNIX_EPOCH},
};

use anyhow::Result;
use flate2::{Compression, GzBuilder};
use fxprof_processed_profile::{
    CategoryHandle, CpuDelta, FrameAddress, FrameFlags, LibraryHandle, LibraryInfo, Profile,
    ProfileFormat, SamplingInterval, Timestamp,
};
use wholesym::samply_symbols::DebugIdExt;

use crate::{
    FirefoxProfileFormat, pprof::atomic_write, profile::TimedStackSample, symbol::Symbolizer,
};

pub fn write_firefox_profile(
    path: &Path,
    samples: &[TimedStackSample],
    format: FirefoxProfileFormat,
    started_unix_nanos: i64,
    frequency: u32,
    process_name: &str,
    mut symbolizer: Option<&mut Symbolizer>,
) -> Result<()> {
    let reference = UNIX_EPOCH
        + Duration::from_nanos(u64::try_from(started_unix_nanos.max(0)).unwrap_or(u64::MAX));
    let interval = SamplingInterval::from_nanos(1_000_000_000_u64 / u64::from(frequency.max(1)));
    let mut profile = Profile::new("rustprofile", reference.into(), interval);
    profile.set_os_name("Linux");
    let first_timestamp = samples
        .iter()
        .map(|sample| sample.timestamp)
        .min()
        .unwrap_or(0);
    let mut processes = HashMap::new();
    let mut threads = HashMap::new();
    let mut libraries = HashMap::<(String, Option<String>), LibraryHandle>::new();
    let mut process_mappings = HashSet::new();

    for sample in samples {
        let process = *processes.entry(sample.pid).or_insert_with(|| {
            profile.add_process(
                process_name,
                sample.pid,
                Timestamp::from_nanos_since_reference(0),
            )
        });
        let thread = *threads.entry((sample.pid, sample.tid)).or_insert_with(|| {
            let thread = profile.add_thread(
                process,
                sample.tid,
                Timestamp::from_nanos_since_reference(0),
                sample.tid == sample.pid,
            );
            if let Some(name) = sample.thread_name.as_deref() {
                profile.set_thread_name(thread, name);
            }
            thread
        });
        let mut frames = sample.stack.0.iter().rev();
        let stack = profile.handle_for_stack_frames(|profile| {
            let frame = frames.next()?;
            let is_leaf = frames.len() == 0;
            if let Some(mapping) = symbolizer
                .as_deref_mut()
                .and_then(|symbolizer| symbolizer.mapping_for_address(frame.address))
            {
                let path = mapping.filename.to_string_lossy().into_owned();
                if path.starts_with("[jit:")
                    && let Some(name) = symbolizer.as_deref_mut().and_then(|symbolizer| {
                        symbolizer
                            .resolve(frame.address)
                            .lines
                            .first()
                            .map(|line| line.function.clone())
                    })
                {
                    let label = profile.handle_for_string(&name);
                    return Some(profile.handle_for_frame_with_label(
                        label,
                        CategoryHandle::OTHER,
                        FrameFlags::empty(),
                    ));
                }
                let library_key = (path.clone(), mapping.build_id.clone());
                let library = *libraries.entry(library_key).or_insert_with(|| {
                    let name = mapping
                        .filename
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or(&path)
                        .to_owned();
                    let debug_id = mapping
                        .build_id
                        .as_deref()
                        .and_then(|build_id| hex::decode(build_id).ok())
                        .map(|build_id| {
                            fxprof_processed_profile::debugid::DebugId::from_identifier(
                                &build_id, true,
                            )
                        })
                        .unwrap_or_default();
                    profile.add_lib(LibraryInfo {
                        name: name.clone(),
                        debug_name: name,
                        path: path.clone(),
                        debug_path: path,
                        debug_id,
                        code_id: mapping.build_id.clone(),
                        arch: None,
                    })
                });
                if process_mappings.insert((sample.pid, mapping.start, mapping.limit)) {
                    profile.add_lib_mapping(
                        process,
                        library,
                        mapping.start,
                        mapping.limit,
                        mapping.relative_address_at_start,
                    );
                }
            }
            let address = if is_leaf {
                FrameAddress::InstructionPointer(process, frame.address)
            } else {
                FrameAddress::ReturnAddress(process, frame.address)
            };
            Some(profile.handle_for_frame_with_address(
                address,
                CategoryHandle::OTHER,
                FrameFlags::empty(),
            ))
        });
        profile.add_sample(
            thread,
            Timestamp::from_nanos_since_reference(sample.timestamp.saturating_sub(first_timestamp)),
            stack,
            CpuDelta::from_nanos(sample.cpu_delta),
            1,
        );
    }

    atomic_write(path, |file| {
        let filename = path
            .file_stem()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "profile".to_owned());
        let encoder = GzBuilder::new()
            .filename(filename)
            .write(file, Compression::new(2));
        let writer = BufWriter::new(encoder);
        profile.to_writer(
            writer,
            match format {
                FirefoxProfileFormat::Json => ProfileFormat::Json,
                FirefoxProfileFormat::Jslb => ProfileFormat::JsonSlabs,
            },
        )?;
        Ok(())
    })
}
