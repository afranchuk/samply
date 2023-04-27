use framehop::{Module, Unwinder};
use fxprof_processed_profile::{
    CounterHandle, LibraryHandle, MarkerTiming, ProcessHandle, Profile, ThreadHandle, Timestamp,
};

use super::process_threads::ProcessThreads;
use super::thread::Thread;

use crate::shared::jit_category_manager::JitCategoryManager;
use crate::shared::jit_function_add_marker::JitFunctionAddMarker;
use crate::shared::jit_function_recycler::JitFunctionRecycler;
use crate::shared::jitdump_manager::JitDumpManager;
use crate::shared::lib_mappings::{LibMappingAdd, LibMappingInfo, LibMappingOp, LibMappingOpQueue};
use crate::shared::perf_map::try_load_perf_map;
use crate::shared::process_sample_data::ProcessSampleData;
use crate::shared::recycling::{ProcessRecyclingData, ThreadRecycler};
use crate::shared::timestamp_converter::TimestampConverter;

use crate::shared::unresolved_samples::UnresolvedSamples;

pub struct Process<U>
where
    U: Unwinder<Module = Module<Vec<u8>>> + Default,
{
    pub profile_process: ProcessHandle,
    pub unwinder: U,
    pub jitdump_manager: JitDumpManager,
    pub lib_mapping_ops: LibMappingOpQueue,
    pub name: Option<String>,
    pub threads: ProcessThreads,
    pub pid: i32,
    pub unresolved_samples: UnresolvedSamples,
    pub jit_function_recycler: Option<JitFunctionRecycler>,
    pub prev_mm_filepages_size: i64,
    pub prev_mm_anonpages_size: i64,
    pub prev_mm_swapents_size: i64,
    pub prev_mm_shmempages_size: i64,
    pub mem_counter: Option<CounterHandle>,
}

impl<U> Process<U>
where
    U: Unwinder<Module = Module<Vec<u8>>> + Default,
{
    pub fn new(
        pid: i32,
        process_handle: ProcessHandle,
        main_thread_handle: ThreadHandle,
        name: Option<String>,
        thread_recycler: Option<ThreadRecycler>,
        jit_function_recycler: Option<JitFunctionRecycler>,
    ) -> Self {
        Self {
            profile_process: process_handle,
            unwinder: U::default(),
            jitdump_manager: JitDumpManager::new_for_process(main_thread_handle),
            lib_mapping_ops: Default::default(),
            name,
            pid,
            threads: ProcessThreads::new(pid, process_handle, main_thread_handle, thread_recycler),
            jit_function_recycler,
            unresolved_samples: Default::default(),
            prev_mm_filepages_size: 0,
            prev_mm_anonpages_size: 0,
            prev_mm_swapents_size: 0,
            prev_mm_shmempages_size: 0,
            mem_counter: None,
        }
    }

    pub fn swap_recycling_data(
        &mut self,
        recycling_data: ProcessRecyclingData,
    ) -> Option<ProcessRecyclingData> {
        let ProcessRecyclingData {
            process_handle,
            main_thread_handle,
            thread_recycler,
            jit_function_recycler,
        } = recycling_data;
        let old_process_handle = std::mem::replace(&mut self.profile_process, process_handle);
        let old_jit_function_recycler =
            std::mem::replace(&mut self.jit_function_recycler, Some(jit_function_recycler));
        let (old_main_thread_handle, old_thread_recycler) = self.threads.swap_recycling_data(
            process_handle,
            main_thread_handle,
            thread_recycler,
        )?;
        Some(ProcessRecyclingData {
            process_handle: old_process_handle,
            main_thread_handle: old_main_thread_handle,
            thread_recycler: old_thread_recycler,
            jit_function_recycler: old_jit_function_recycler?,
        })
    }

    pub fn set_name(&mut self, name: String, profile: &mut Profile) {
        profile.set_process_name(self.profile_process, &name);
        self.threads.main_thread.set_name(name.clone(), profile);
        self.name = Some(name);
    }

    pub fn recycle_or_get_new_thread(
        &mut self,
        tid: i32,
        name: Option<String>,
        start_time: Timestamp,
        profile: &mut Profile,
    ) -> &mut Thread {
        self.threads
            .recycle_or_get_new_thread(tid, name, start_time, profile)
    }

    pub fn check_jitdump(
        &mut self,
        jit_category_manager: &mut JitCategoryManager,
        profile: &mut Profile,
        timestamp_converter: &TimestampConverter,
    ) {
        self.jitdump_manager.process_pending_records(
            jit_category_manager,
            profile,
            self.jit_function_recycler.as_mut(),
            timestamp_converter,
        );
    }

    pub fn notify_dead(&mut self, end_time: Timestamp, profile: &mut Profile) {
        self.threads.notify_process_dead(end_time, profile);
        profile.set_process_end_time(self.profile_process, end_time);
    }

    pub fn finish(
        mut self,
        profile: &mut Profile,
        jit_category_manager: &mut JitCategoryManager,
        timestamp_converter: &TimestampConverter,
    ) -> (ProcessSampleData, Option<(String, ProcessRecyclingData)>) {
        self.unwinder = U::default();

        let perf_map_mappings = if !self.unresolved_samples.is_empty() {
            try_load_perf_map(
                self.pid as u32,
                profile,
                jit_category_manager,
                self.jit_function_recycler.as_mut(),
            )
        } else {
            None
        };

        let jitdump_manager = std::mem::replace(
            &mut self.jitdump_manager,
            JitDumpManager::new_for_process(self.threads.main_thread.profile_thread),
        );
        let jitdump_ops = jitdump_manager.finish(
            jit_category_manager,
            profile,
            self.jit_function_recycler.as_mut(),
            timestamp_converter,
        );

        let process_sample_data = ProcessSampleData::new(
            std::mem::take(&mut self.unresolved_samples),
            std::mem::take(&mut self.lib_mapping_ops),
            jitdump_ops,
            perf_map_mappings,
        );

        let thread_recycler = self.threads.finish();

        let process_recycling_data = if let (
            Some(name),
            Some(mut jit_function_recycler),
            (main_thread_handle, Some(thread_recycler)),
        ) = (self.name, self.jit_function_recycler, thread_recycler)
        {
            jit_function_recycler.finish_round();
            let recycling_data = ProcessRecyclingData {
                process_handle: self.profile_process,
                main_thread_handle,
                thread_recycler,
                jit_function_recycler,
            };
            Some((name, recycling_data))
        } else {
            None
        };

        (process_sample_data, process_recycling_data)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_regular_lib_mapping(
        &mut self,
        timestamp: u64,
        start_address: u64,
        end_address: u64,
        relative_address_at_start: u32,
        lib_handle: LibraryHandle,
    ) {
        self.lib_mapping_ops.push(
            timestamp,
            LibMappingOp::Add(LibMappingAdd {
                start_avma: start_address,
                end_avma: end_address,
                relative_address_at_start,
                info: LibMappingInfo::new_lib(lib_handle),
            }),
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_lib_mapping_for_injected_jit_lib(
        &mut self,
        timestamp: u64,
        profile_timestamp: Timestamp,
        symbol_name: Option<&str>,
        start_address: u64,
        end_address: u64,
        mut relative_address_at_start: u32,
        mut lib_handle: LibraryHandle,
        jit_category_manager: &mut JitCategoryManager,
        profile: &mut Profile,
    ) {
        let main_thread = self.threads.main_thread.profile_thread;
        let timing = MarkerTiming::Instant(profile_timestamp);
        profile.add_marker(
            main_thread,
            "JitFunctionAdd",
            JitFunctionAddMarker(symbol_name.unwrap_or("<unknown>").to_owned()),
            timing,
        );

        if let (Some(name), Some(recycler)) = (symbol_name, self.jit_function_recycler.as_mut()) {
            (lib_handle, relative_address_at_start) = recycler.recycle(
                start_address,
                end_address,
                relative_address_at_start,
                name,
                lib_handle,
            );
        }

        let (category, js_frame) =
            jit_category_manager.classify_jit_symbol(symbol_name.unwrap_or(""), profile);
        self.lib_mapping_ops.push(
            timestamp,
            LibMappingOp::Add(LibMappingAdd {
                start_avma: start_address,
                end_avma: end_address,
                relative_address_at_start,
                info: LibMappingInfo::new_jit_function(lib_handle, category, js_frame),
            }),
        );
    }

    pub fn get_or_make_mem_counter(&mut self, profile: &mut Profile) -> CounterHandle {
        *self.mem_counter.get_or_insert_with(|| {
            profile.add_counter(
                self.profile_process,
                "malloc",
                "Memory",
                "Amount of allocated memory",
            )
        })
    }
}
