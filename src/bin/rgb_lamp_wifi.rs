//! The example implements an RGB Light device.
#![no_std]
#![no_main]
#![recursion_limit = "256"]

use core::pin::pin;

use embassy_executor::Spawner;

use esp_alloc::heap_allocator;
use esp_backtrace as _;
use esp_hal::analog::adc::{Adc, AdcConfig, Attenuation};
use esp_hal::gpio::{Input, InputConfig, Pull};
use esp_hal::timer::timg::TimerGroup;
use esp_metadata_generated::memory_range;
use esp_storage::FlashStorage;

use embassy_embedded_hal::adapter::BlockingAsync;
use embassy_futures::select::{Either, Either3, select, select3};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::Timer;

use rs_matter_embassy::epoch::epoch;
use rs_matter_embassy::matter::dm::clusters::desc::{self, ClusterHandler as _};
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
use rs_matter_embassy::matter::tlv::Nullable;
use rs_matter_embassy::matter::utils::init::InitMaybeUninit;
use rs_matter_embassy::matter::{clusters, devices};
use rs_matter_embassy::persist::EmbassyKvBlobStore;
use rs_matter_embassy::rand::esp::{esp_init_rand, esp_rand};
use rs_matter_embassy::stack::persist::KvBlobStore;
use rs_matter_embassy::wireless::esp::EspWifiDriver;
use rs_matter_embassy::wireless::{EmbassyWifi, EmbassyWifiMatterStack};

use matter_rgb_lamp::dm::color_control::{self, ClusterHandler as _};
use matter_rgb_lamp::led::led_driver;
use matter_rgb_lamp::led::led_handler::LedHandler;
use matter_rgb_lamp::logging::{error, info};

extern crate alloc;

macro_rules! mk_static {
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

fn get_persistent_store() -> impl KvBlobStore {
    use esp_bootloader_esp_idf::partitions::{
        DataPartitionSubType, PARTITION_TABLE_MAX_LEN, PartitionType, read_partition_table,
    };

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

#[cfg(feature = "defmt")]
use esp_println as _;

#[esp_rtos::main]
async fn main(_s: Spawner) {
    #[cfg(feature = "log")]
    esp_println::logger::init_logger(log::LevelFilter::Info);

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
    let channel = Channel::<CriticalSectionRawMutex, led_driver::ControlMessage, 4>::new();
    let sender = channel.sender();

    let button_on_off = Input::new(
        peripherals.GPIO7,
        InputConfig::default().with_pull(Pull::Up),
    );

    let mut adc1_config = AdcConfig::new();
    let pin = adc1_config.enable_pin(peripherals.GPIO4, Attenuation::_11dB);
    let adc1 = Adc::new(peripherals.ADC1, adc1_config);

    let led_handler = LedHandler::new(sender, button_on_off, adc1, pin);

    let on_off_handler = OnOffHandler::new(
        Dataver::new_rand(stack.matter().rand()),
        LIGHT_ENDPOINT_ID,
        &led_handler,
    );
    let level_control_handler = LevelControlHandler::new(
        Dataver::new_rand(stack.matter().rand()),
        LIGHT_ENDPOINT_ID,
        &led_handler,
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
                Some(OnOffHandler::<LedHandler, LedHandler>::CLUSTER.id),
            ),
            on_off::HandlerAsyncAdaptor(&on_off_handler),
        )
        .chain(
            EpClMatcher::new(
                Some(LIGHT_ENDPOINT_ID),
                Some(LevelControlHandler::<LedHandler, LedHandler>::CLUSTER.id),
            ),
            level_control::HandlerAsyncAdaptor(&level_control_handler),
        )
        .chain(
            EpClMatcher::new(
                Some(LIGHT_ENDPOINT_ID),
                Some(color_control::ColorControlHandler::<LedHandler>::CLUSTER.id),
            ),
            Async(
                color_control::ColorControlHandler::new(
                    Dataver::new_rand(stack.matter().rand()),
                    &led_handler,
                )
                .adapt(),
            ),
        )
        .chain(
            EpClMatcher::new(Some(LIGHT_ENDPOINT_ID), Some(desc::DescHandler::CLUSTER.id)),
            Async(desc::DescHandler::new(Dataver::new_rand(stack.matter().rand())).adapt()),
        );

    // == Step 4: ==
    // Run the Matter stack with our handler
    // Using `pin!` is completely optional, but reduces the size of the final future

    // Create the persister & load any previously saved state
    // `EmbassyPersist`+`EmbassyKvBlobStore` saves to a user-supplied NOR Flash region
    // However, for this demo and for simplicity, we use a dummy persister that does nothing
    let persist = stack
        .create_persist_with_comm_window(get_persistent_store())
        .await
        .unwrap();

    // This step can be repeated in that the stack can be stopped and started multiple times, as needed.
    let mut matter = pin!(stack.run_coex(
        // The Matter stack needs to instantiate an `embassy-net` `Driver` and `Controller`
        EmbassyWifi::new(
            EspWifiDriver::new(&init, peripherals.WIFI, peripherals.BT),
            stack
        ),
        // The Matter stack needs a persister to store its state
        &persist,
        // Our `AsyncHandler` + `AsyncMetadata` impl
        (NODE, handler),
        // User future to run; the LED task
        (),
    ));

    // == Step 5: ==
    // Setup the LED driver
    let receiver = channel.receiver();
    let led_driver = led_driver::Driver::new(peripherals.RMT, peripherals.GPIO8.into(), receiver);
    let mut led_task = pin!(led_driver.run());

    // == Step 6: ==
    // Setup Factory Reset button
    let mut button_reset = Input::new(
        peripherals.GPIO9,
        InputConfig::default().with_pull(Pull::Up),
    );

    // Hold for 3 seconds to initiate a factory reset
    let mut reset_button_task = async || {
        loop {
            button_reset.wait_for_falling_edge().await;
            match select(button_reset.wait_for_rising_edge(), Timer::after_secs(3)).await {
                Either::First(_) => (),
                Either::Second(_) => {
                    info!("Performing factory reset...");
                    if let Err(e) = persist.reset().await {
                        error!("Factory reset error: {}", e);
                    };
                    // todo reset non-volatile attributes.
                    // todo Consider adding a `reset()` method to the rs-matter handlers.
                }
            }
        }
    };

    // == Step 7: ==
    // Run async tasks
    match select3(&mut matter, &mut led_task, &mut pin!(reset_button_task())).await {
        Either3::First(r) => {
            panic!("Matter thread exited! {:?}", r)
        }
        Either3::Second(_) => {
            panic!("LED thread exited!")
        }
        Either3::Third(_) => {
            panic!("Reset button thread exited!")
        }
    }
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
                desc::DescHandler::CLUSTER,
                OnOffHandler::<LedHandler, LedHandler>::CLUSTER,
                LevelControlHandler::<LedHandler, LedHandler>::CLUSTER
                color_control::ColorControlHandler::<LedHandler>::CLUSTER
            ),
        },
    ],
};
