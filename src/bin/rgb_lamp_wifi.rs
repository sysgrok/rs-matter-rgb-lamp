//! The example implements an RGB Light device.
#![no_std]
#![no_main]
#![recursion_limit = "256"]

use core::pin::pin;

use embassy_executor::Spawner;

use embassy_futures::select::select;
use esp_alloc::heap_allocator;
use esp_backtrace as _;
use esp_hal::peripherals::{ADC1, GPIO4};
use esp_hal::timer::timg::TimerGroup;
use esp_metadata_generated::memory_range;

use rs_matter_embassy::epoch::epoch;
use rs_matter_embassy::matter::dm::clusters::desc::{ClusterHandler as _, DescHandler};
use rs_matter_embassy::matter::dm::clusters::level_control::{
    self, AttributeDefaults, ClusterAsyncHandler as _, LevelControlHandler, OptionsBitmap,
};
use rs_matter_embassy::matter::dm::clusters::on_off::{
    self, ClusterAsyncHandler as _, OnOffHandler,
};
use rs_matter_embassy::matter::dm::devices::test::{TEST_DEV_ATT, TEST_DEV_COMM, TEST_DEV_DET};
use rs_matter_embassy::matter::dm::{
    Async, Dataver, DeviceType, EmptyHandler, Endpoint, EpClMatcher, Node,
};
use rs_matter_embassy::matter::error::Error;
use rs_matter_embassy::matter::tlv::Nullable;
use rs_matter_embassy::matter::utils::init::InitMaybeUninit;
use rs_matter_embassy::matter::utils::select::Coalesce;
use rs_matter_embassy::matter::{clusters, devices};
use rs_matter_embassy::rand::esp::{esp_init_rand, esp_rand};
use rs_matter_embassy::stack::persist::{KvBlobStore, MatterPersist, NetworkPersist};
use rs_matter_embassy::wireless::esp::EspWifiDriver;
use rs_matter_embassy::wireless::{EmbassyWifi, EmbassyWifiMatterStack};

use matter_rgb_lamp::dm::color_control::{ClusterAsyncHandler as _, ColorControlHandler};
use matter_rgb_lamp::led::Led;
use matter_rgb_lamp::logging::{info, warn};

extern crate alloc;

macro_rules! mk_static {
    ($t:ty, $v:expr) => {{
        #[cfg(not(feature = "esp32"))]
        {
            static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
            STATIC_CELL.uninit().write($v)
        }
        #[cfg(feature = "esp32")]
        alloc::boxed::Box::leak(alloc::boxed::Box::<$t>::new($v))
    }};
    ($t:ty) => {{
        #[cfg(not(feature = "esp32"))]
        {
            static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
            STATIC_CELL.uninit()
        }
        #[cfg(feature = "esp32")]
        alloc::boxed::Box::leak(alloc::boxed::Box::<$t>::new_uninit())
    }};
}

/// The amount of memory for allocating all `rs-matter-stack` futures created during
/// the execution of the `run*` methods.
/// This does NOT include the rest of the Matter stack.
///
/// The futures of `rs-matter-stack` created during the execution of the `run*` methods
/// are allocated in a special way using a small bump allocator which results
/// in a much lower memory usage by those.
///
/// If - for your platform - this size is not enough, increase it until
/// the program runs without panics during the stack initialization.
const BUMP_SIZE: usize = 18000;

/// Heap strictly necessary only for Wifi+BLE and for the only Matter dependency which needs (~4KB) alloc - `x509`
#[cfg(not(feature = "esp32"))]
const HEAP_SIZE: usize = 100 * 1024;
/// On the esp32, we allocate the Matter Stack from heap as well, due to the non-contiguous memory regions on that chip
#[cfg(feature = "esp32")]
const HEAP_SIZE: usize = 140 * 1024;

const RECLAIMED_RAM: usize =
    memory_range!("DRAM2_UNINIT").end - memory_range!("DRAM2_UNINIT").start;

esp_bootloader_esp_idf::esp_app_desc!();

#[cfg(feature = "defmt")]
use esp_println as _;

type LedHw<'a> = Led<'a, ADC1<'a>, GPIO4<'a>>;

#[esp_rtos::main]
async fn main(_s: Spawner) {
    #[cfg(feature = "log")]
    esp_println::logger::init_logger_from_env();

    info!("Starting...");

    heap_allocator!(size: HEAP_SIZE - RECLAIMED_RAM);
    heap_allocator!(#[esp_hal::ram(reclaimed)] size: RECLAIMED_RAM);

    // == Step 1: ==
    // Necessary `esp-hal` and `esp-wifi` initialization boilerplate

    let peripherals = esp_hal::init(esp_hal::Config::default());

    // To erase generics, `Matter` takes a rand `fn` rather than a trait or a closure,
    // so we need to initialize the global `rand` fn once
    esp_init_rand(esp_hal::rng::Rng::new());

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(
        timg0.timer0,
        #[cfg(target_arch = "riscv32")]
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT)
            .software_interrupt0,
    );

    let init = esp_radio::init().unwrap();

    // == Step 2: ==
    // Allocate the Matter stack.
    // For MCUs, it is best to allocate it statically, so as to avoid program stack blowups (its memory footprint is ~ 35 to 50KB).
    // It is also (currently) a mandatory requirement when the wireless stack variation is used.
    let stack = mk_static!(EmbassyWifiMatterStack::<BUMP_SIZE, ()>).init_with(
        EmbassyWifiMatterStack::init(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, epoch, esp_rand),
    );

    // == Step 3: ==
    // Set up Matter data model handler

    // Setup our hardware
    let led = LedHw::new(
        peripherals.RMT,
        peripherals.GPIO8,
        peripherals.GPIO9,
        #[cfg(feature = "adc")]
        {
            Some((peripherals.ADC1, peripherals.GPIO4))
        },
        #[cfg(not(feature = "adc"))]
        {
            Option::<(ADC1, GPIO4)>::None
        },
    );

    let on_off_handler = OnOffHandler::new(
        Dataver::new_rand(stack.matter().rand()),
        LIGHT_ENDPOINT_ID,
        &led,
    );
    let level_control_handler = LevelControlHandler::new(
        Dataver::new_rand(stack.matter().rand()),
        LIGHT_ENDPOINT_ID,
        &led,
        AttributeDefaults {
            on_level: Nullable::none(),
            options: OptionsBitmap::EXECUTE_IF_OFF,
            ..Default::default()
        },
    );

    on_off_handler.init(Some(&level_control_handler));
    level_control_handler.init(Some(&on_off_handler));

    // Chain our endpoint clusters
    let handler = EmptyHandler
        .chain(
            EpClMatcher::new(
                Some(LIGHT_ENDPOINT_ID),
                Some(OnOffHandler::<LedHw, LedHw>::CLUSTER.id),
            ),
            on_off::HandlerAsyncAdaptor(&on_off_handler),
        )
        .chain(
            EpClMatcher::new(
                Some(LIGHT_ENDPOINT_ID),
                Some(LevelControlHandler::<LedHw, LedHw>::CLUSTER.id),
            ),
            level_control::HandlerAsyncAdaptor(&level_control_handler),
        )
        .chain(
            EpClMatcher::new(
                Some(LIGHT_ENDPOINT_ID),
                Some(ColorControlHandler::<LedHw>::CLUSTER.id),
            ),
            ColorControlHandler::new(Dataver::new_rand(stack.matter().rand()), &led).adapt(),
        )
        .chain(
            EpClMatcher::new(Some(LIGHT_ENDPOINT_ID), Some(DescHandler::CLUSTER.id)),
            Async(DescHandler::new(Dataver::new_rand(stack.matter().rand())).adapt()),
        );

    // == Step 4: ==
    // Run the Matter stack with our handler

    // Create the persister & load any previously saved state
    // `EmbassyPersist`+`EmbassyKvBlobStore` saves to a user-supplied NOR Flash region
    // However, for this demo and for simplicity, we use a dummy persister that does nothing
    let persist = stack
        .create_persist_with_comm_window(create_blob_store())
        .await
        .unwrap();

    // This step can be repeated in that the stack can be stopped and started multiple times, as needed.
    let mut matter_task = pin!(stack.run_coex(
        // The Matter stack needs to instantiate an `embassy-net` `Driver` and `Controller`
        EmbassyWifi::new(
            EspWifiDriver::new(&init, peripherals.WIFI, peripherals.BT),
            stack,
        ),
        // The Matter stack needs a persister to store its state
        &persist,
        // Our `AsyncHandler` + `AsyncMetadata` impl
        (NODE, handler),
        // User future to run; the LED task
        (),
    ));

    let mut reset_task = pin!(factory_reset(&led, &persist));

    select(&mut matter_task, &mut reset_task)
        .coalesce()
        .await
        .unwrap();
}

async fn factory_reset<S, C>(
    led: &LedHw<'_>,
    persist: &MatterPersist<'_, S, C>,
) -> Result<(), Error>
where
    S: KvBlobStore,
    C: NetworkPersist,
{
    loop {
        led.wait_factory_reset().await;

        warn!("Performing factory reset...");

        let _ = persist.reset().await;

        info!("Factory reset complete. Restart the device to re-provision.");
    }
}

#[cfg(feature = "persist")]
fn create_blob_store() -> impl KvBlobStore {
    use embassy_embedded_hal::adapter::BlockingAsync;
    use esp_bootloader_esp_idf::partitions::{
        DataPartitionSubType, PARTITION_TABLE_MAX_LEN, PartitionType, read_partition_table,
    };
    use esp_storage::FlashStorage;
    use rs_matter_embassy::persist::EmbassyKvBlobStore;

    let mut flash = FlashStorage::new();
    let mut pt_mem = [0u8; PARTITION_TABLE_MAX_LEN];
    let pt = read_partition_table(&mut flash, &mut pt_mem).unwrap();
    let nvs = pt
        .find_partition(PartitionType::Data(DataPartitionSubType::Nvs))
        .unwrap()
        .unwrap();

    let start = nvs.offset();
    let end = nvs.offset() + nvs.len();
    info!("Found persistent partition at {:#x}..{:#x}", start, end);

    EmbassyKvBlobStore::new(BlockingAsync::new(flash), start..end)
}

#[cfg(not(feature = "persist"))]
fn create_blob_store() -> impl KvBlobStore {
    rs_matter_embassy::stack::persist::DummyKvBlobStore
}

/// Endpoint 0 (the root endpoint) always runs
/// the hidden Matter system clusters, so we pick ID=1
const LIGHT_ENDPOINT_ID: u16 = 1;

const DEV_TYPE_ENHANCED_COLOR_LIGHT: DeviceType = DeviceType {
    dtype: 0x010D,
    drev: 4,
};

/// The Matter Light device Node
const NODE: Node = Node {
    id: 0,
    endpoints: &[
        EmbassyWifiMatterStack::<0, ()>::root_endpoint(),
        Endpoint {
            id: LIGHT_ENDPOINT_ID,
            device_types: devices!(DEV_TYPE_ENHANCED_COLOR_LIGHT),
            clusters: clusters!(
                DescHandler::CLUSTER,
                OnOffHandler::<LedHw, LedHw>::CLUSTER,
                LevelControlHandler::<LedHw, LedHw>::CLUSTER
                ColorControlHandler::<LedHw>::CLUSTER
            ),
        },
    ],
};
