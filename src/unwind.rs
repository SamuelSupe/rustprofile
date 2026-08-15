use std::{collections::BTreeMap, fs, path::PathBuf};

use anyhow::Result;
use framehop::{CacheNative, MayAllocateDuringUnwind, Module, Unwinder, UnwinderNative};
use framehop_object::ObjectSectionInfo;
use object::{Object, ObjectSegment};

use crate::{
    config::DEFAULT_MAX_FRAMES,
    maps::{ExecutableRanges, MapEntry, read_process_maps},
    process::mapped_module_path,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct RawRegisters {
    pub ip: u64,
    pub sp: u64,
    pub fp: u64,
    #[cfg_attr(target_arch = "x86_64", allow(dead_code))]
    pub lr: u64,
}

#[derive(Clone, Debug)]
pub struct StackSnapshot {
    pub registers: RawRegisters,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct UnwindOutcome {
    pub frames: Vec<u64>,
    pub truncated: bool,
    pub error: Option<String>,
    pub fatal: bool,
}

pub struct DwarfUnwinder {
    unwinder: UnwinderNative<Vec<u8>, MayAllocateDuringUnwind>,
    cache: CacheNative<MayAllocateDuringUnwind>,
    executable_ranges: ExecutableRanges,
    #[cfg(target_arch = "aarch64")]
    max_known_code_address: u64,
    #[cfg(target_arch = "aarch64")]
    code_pointer_mask: Option<u64>,
}

impl DwarfUnwinder {
    pub fn for_process(pid: i32) -> Result<Self> {
        Self::from_maps(pid, &read_process_maps(pid)?)
    }

    pub fn from_maps(pid: i32, maps: &[MapEntry]) -> Result<Self> {
        let mut unwinder = UnwinderNative::new();
        let mut modules = BTreeMap::<PathBuf, Vec<&MapEntry>>::new();
        let executable_ranges = ExecutableRanges::from_maps(maps);

        for mapping in maps
            .iter()
            .filter(|mapping| mapping.inode != 0 && mapping.is_executable())
        {
            if let Some(path) = &mapping.path {
                modules.entry(path.clone()).or_default().push(mapping);
            }
        }

        for (path, mappings) in modules {
            let start = mappings
                .iter()
                .map(|mapping| mapping.start)
                .min()
                .unwrap_or(0);
            let end = mappings
                .iter()
                .map(|mapping| mapping.end)
                .max()
                .unwrap_or(0);
            if start >= end {
                continue;
            }

            let process_path = mapped_module_path(pid, &path, mappings.first().copied());
            let Ok(data) = fs::read(&process_path) else {
                continue;
            };
            let Ok(object) = object::File::parse(data.as_slice()) else {
                continue;
            };
            let base_avma = elf_base_avma(&object, &mappings).unwrap_or_else(|| {
                mappings
                    .iter()
                    .filter_map(|mapping| mapping.start.checked_sub(mapping.offset))
                    .min()
                    .unwrap_or(start)
            });
            let section_info = ObjectSectionInfo::from_ref(&object);
            let module = Module::<Vec<u8>>::new(
                path.to_string_lossy().into_owned(),
                start..end,
                base_avma,
                section_info,
            );
            unwinder.add_module(module);
        }

        #[cfg(target_arch = "aarch64")]
        let max_known_code_address = executable_ranges.max_address();
        Ok(Self {
            unwinder,
            cache: CacheNative::new(),
            executable_ranges,
            #[cfg(target_arch = "aarch64")]
            max_known_code_address,
            #[cfg(target_arch = "aarch64")]
            code_pointer_mask: kernel_code_pointer_mask(pid),
        })
    }

    pub fn unwind(&mut self, snapshot: &StackSnapshot) -> UnwindOutcome {
        self.unwind_bytes(snapshot.registers, &snapshot.bytes)
    }

    pub fn unwind_bytes(&mut self, registers: RawRegisters, stack_bytes: &[u8]) -> UnwindOutcome {
        let mut outcome = UnwindOutcome::default();
        let Some(ip) = self.normalize_code_address(registers.ip) else {
            outcome.error = Some(format!(
                "instruction pointer {:#x} does not resolve uniquely to executable memory",
                registers.ip
            ));
            outcome.fatal = cfg!(target_arch = "aarch64");
            return outcome;
        };

        let stack_start = registers.sp;
        let stack_end = stack_start.saturating_add(stack_bytes.len() as u64);
        let mut read_stack = |address: u64| -> std::result::Result<u64, ()> {
            let end = address.checked_add(8).ok_or(())?;
            if address < stack_start || end > stack_end {
                return Err(());
            }
            let offset = usize::try_from(address - stack_start).map_err(|_| ())?;
            let bytes: [u8; 8] = stack_bytes
                .get(offset..offset + 8)
                .ok_or(())?
                .try_into()
                .map_err(|_| ())?;
            Ok(u64::from_ne_bytes(bytes))
        };

        #[cfg(target_arch = "x86_64")]
        let unwind_registers = framehop::UnwindRegsNative::new(ip, registers.sp, registers.fp);
        #[cfg(target_arch = "aarch64")]
        let unwind_registers = {
            use framehop::aarch64::PtrAuthMask;
            let mask = self.code_pointer_mask.map(PtrAuthMask).unwrap_or_else(|| {
                PtrAuthMask::from_max_known_address(self.max_known_code_address)
            });
            framehop::UnwindRegsNative::new_with_ptr_auth_mask(
                mask,
                registers.lr,
                registers.sp,
                registers.fp,
            )
        };

        let mut iterator =
            self.unwinder
                .iter_frames(ip, unwind_registers, &mut self.cache, &mut read_stack);
        loop {
            match iterator.next() {
                Ok(Some(frame)) => {
                    let address = frame.address_for_lookup();
                    if self.executable_ranges.contains(address) {
                        outcome.frames.push(address);
                    } else {
                        outcome.error = Some(format!(
                            "unwound address {address:#x} is outside executable mappings"
                        ));
                        outcome.fatal = cfg!(target_arch = "aarch64");
                        break;
                    }
                    if outcome.frames.len() == DEFAULT_MAX_FRAMES {
                        outcome.truncated = true;
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    outcome.error = Some(error.to_string());
                    break;
                }
            }
        }
        outcome
    }

    fn normalize_code_address(&self, address: u64) -> Option<u64> {
        #[cfg(target_arch = "x86_64")]
        {
            self.executable_ranges.contains(address).then_some(address)
        }

        #[cfg(target_arch = "aarch64")]
        {
            use framehop::aarch64::PtrAuthMask;
            let guessed = PtrAuthMask::from_max_known_address(self.max_known_code_address)
                .strip_ptr_auth(address);
            let kernel = self.code_pointer_mask.map(|mask| address & mask);
            let mut resolved = None;
            for candidate in [
                Some(address),
                Some(address & 0x00ff_ffff_ffff_ffff),
                Some(guessed),
                kernel,
            ]
            .into_iter()
            .flatten()
            {
                if !self.executable_ranges.contains(candidate) {
                    continue;
                }
                match resolved {
                    None => resolved = Some(candidate),
                    Some(previous) if previous == candidate => {}
                    Some(_) => return None,
                }
            }
            resolved
        }
    }
}

#[cfg(all(not(target_os = "linux"), target_arch = "aarch64"))]
fn kernel_code_pointer_mask(_pid: i32) -> Option<u64> {
    None
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
fn kernel_code_pointer_mask(pid: i32) -> Option<u64> {
    const NT_ARM_PAC_MASK: usize = 0x406;
    // SAFETY: PTRACE_SEIZE is invoked with a numeric PID, null address and no options.
    let seized = unsafe {
        libc::ptrace(
            libc::PTRACE_SEIZE,
            pid,
            std::ptr::null_mut::<libc::c_void>(),
            std::ptr::null_mut::<libc::c_void>(),
        )
    };
    if seized != 0 {
        return None;
    }
    struct Detach(i32);
    impl Drop for Detach {
        fn drop(&mut self) {
            // SAFETY: this guard is only created after PTRACE_SEIZE succeeds.
            unsafe {
                libc::ptrace(
                    libc::PTRACE_DETACH,
                    self.0,
                    std::ptr::null_mut::<libc::c_void>(),
                    std::ptr::null_mut::<libc::c_void>(),
                );
            }
        }
    }
    let _detach = Detach(pid);
    // SAFETY: the target is attached through PTRACE_SEIZE above.
    let interrupted = unsafe {
        libc::ptrace(
            libc::PTRACE_INTERRUPT,
            pid,
            std::ptr::null_mut::<libc::c_void>(),
            std::ptr::null_mut::<libc::c_void>(),
        )
    };
    if interrupted != 0 {
        return None;
    }
    let mut status = 0;
    // SAFETY: status points to writable storage and __WALL waits for the traced task.
    let waited = unsafe { libc::waitpid(pid, &mut status, libc::__WALL) };
    if waited != pid {
        return None;
    }
    let mut masks = [0_u64; 2];
    let mut io = libc::iovec {
        iov_base: masks.as_mut_ptr().cast(),
        iov_len: std::mem::size_of_val(&masks),
    };
    // SAFETY: the target is stopped and io points to a correctly sized user_pac_mask buffer.
    let read = unsafe {
        libc::ptrace(
            libc::PTRACE_GETREGSET,
            pid,
            NT_ARM_PAC_MASK as *mut libc::c_void,
            (&mut io as *mut libc::iovec).cast::<libc::c_void>(),
        )
    };
    if read != 0 || io.iov_len < std::mem::size_of_val(&masks) || masks[1] == 0 {
        return None;
    }
    Some(!masks[1])
}

fn elf_base_avma(object: &object::File<'_>, mappings: &[&MapEntry]) -> Option<u64> {
    const PAGE_MASK: u64 = !0xfff;
    for mapping in mappings {
        for segment in object.segments() {
            let (file_offset, _) = segment.file_range();
            if (file_offset & PAGE_MASK) != (mapping.offset & PAGE_MASK) {
                continue;
            }
            let segment_page = segment.address() & PAGE_MASK;
            let load_bias = mapping.start.checked_sub(segment_page)?;
            return load_bias.checked_add(object.relative_address_base());
        }
    }
    None
}

pub fn require_native_architecture(architecture: &str) -> Result<()> {
    let native = std::env::consts::ARCH;
    if architecture != native {
        anyhow::bail!(
            "target architecture {architecture} does not match profiler architecture {native}"
        );
    }
    Ok(())
}
