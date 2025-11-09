use core::cell::Cell;
use core::iter::once;
use core::ops::{Add, Mul};

use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_time::{Instant, Timer};

use esp_hal::analog::adc::{self, Adc, AdcChannel, AdcConfig, AdcPin, Attenuation};
use esp_hal::gpio::{AnalogPin, Input, InputConfig, InputPin, OutputPin, Pull};
use esp_hal::rmt::{PulseCode, Rmt};
use esp_hal::time::Rate;
use esp_hal::{Async, peripherals};
use esp_hal_smartled::SmartLedsAdapterAsync;

use rs_matter_embassy::matter::dm::Cluster;
use rs_matter_embassy::matter::dm::clusters::level_control::{
    self, LevelControlHooks, OptionsBitmap,
};
use rs_matter_embassy::matter::dm::clusters::on_off::{self, OnOffHooks, StartUpOnOffEnum};
use rs_matter_embassy::matter::error::Error;
use rs_matter_embassy::matter::tlv::Nullable;
use rs_matter_embassy::matter::with;

use palette::white_point::D65;
use palette::{FromColor, Srgb, Yxy};

use smart_leds::{SmartLedsWriteAsync, brightness, gamma};

use crate::dm::color_control::ColorControlHooks;

use crate::logging::{debug, info, warn};

pub struct Led<'a, A, AC> {
    /// LED hardware
    led: Mutex<NoopRawMutex, SmartLedsAdapterAsync<'a, 25>>,
    /// Button hardware
    button: Mutex<NoopRawMutex, Input<'a>>,
    /// ADC hardware
    #[allow(clippy::type_complexity)]
    adc: Mutex<NoopRawMutex, Option<(Adc<'a, A, Async>, AdcPin<AC, A>)>>,
    /// Current state of the LED
    state: Cell<LedState>,
    /// New state of the LED, scheduled to be applied
    state_signal: Signal<NoopRawMutex, LedState>,
    /// Factory Reset signal
    factory_reset_signal: Signal<NoopRawMutex, ()>,
    /// StartUpOnOff attribute value
    start_up_on_off: Cell<Option<StartUpOnOffEnum>>,
    /// StartUpCurrentLevel attribute value
    startup_current_level: Cell<Option<u8>>,
}

impl<'a, A: adc::Instance + adc::RegisterAccess + 'a, AC: AdcChannel + AnalogPin + 'a>
    Led<'a, A, AC>
{
    pub fn new(
        led_rmt: peripherals::RMT<'a>,
        led_pin: impl OutputPin + 'a,
        button_pin: impl InputPin + 'a,
        adc: Option<(A, AC)>,
    ) -> Self {
        // Setup the LED
        // Configure RMT (Remote Control Transceiver) peripheral globally
        // <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/peripherals/rmt.html>
        let rmt = Rmt::new(led_rmt, Rate::from_mhz(80)).unwrap().into_async();

        // We use one of the RMT channels to instantiate a `SmartLedsAdapterAsync` which can
        // be used directly with all `smart_led` implementations
        let led = SmartLedsAdapterAsync::new(
            rmt.channel0,
            led_pin,
            [PulseCode::default(); esp_hal_smartled::buffer_size_async(1)],
        );

        let button = Input::new(button_pin, InputConfig::default().with_pull(Pull::Up));

        let adc = adc.map(|(adc, adc_pin)| {
            let mut adc_config = AdcConfig::new();

            let adc_pin = adc_config.enable_pin(adc_pin, Attenuation::_11dB);
            let adc = Adc::new(adc, adc_config).into_async();

            (adc, adc_pin)
        });

        let this = Self {
            led: Mutex::new(led),
            button: Mutex::new(button),
            adc: Mutex::new(adc),
            state: Cell::new(LedState {
                on: false,
                level: 100,
                // White
                x: 39518,
                y: 21233,
            }),
            state_signal: Signal::new(),
            factory_reset_signal: Signal::new(),
            start_up_on_off: Cell::new(Some(StartUpOnOffEnum::Off)),
            startup_current_level: Cell::new(Some(100)),
        };

        this.state_signal.signal(this.state.get());

        this
    }

    pub async fn wait_factory_reset(&self) {
        self.factory_reset_signal.wait().await;
    }

    fn set_state(&self, state: LedState) {
        if self.state.get() != state {
            self.state.set(state);
            self.state_signal.signal(state);
        }
    }
}

impl<'a, A: adc::Instance + adc::RegisterAccess + 'a, AC: AdcChannel + AnalogPin + 'a> OnOffHooks
    for Led<'a, A, AC>
{
    const CLUSTER: Cluster<'static> = on_off::FULL_CLUSTER
        .with_revision(6)
        .with_features(on_off::Feature::LIGHTING.bits())
        .with_attrs(with!(
            required;
            on_off::AttributeId::OnOff
            | on_off::AttributeId::GlobalSceneControl
            | on_off::AttributeId::OnTime
            | on_off::AttributeId::OffWaitTime
            | on_off::AttributeId::StartUpOnOff
        ))
        .with_cmds(with!(
            on_off::CommandId::Off
                | on_off::CommandId::On
                | on_off::CommandId::Toggle
                | on_off::CommandId::OffWithEffect
                | on_off::CommandId::OnWithRecallGlobalScene
                | on_off::CommandId::OnWithTimedOff
        ));

    fn on_off(&self) -> bool {
        self.state.get().on
    }

    fn set_on_off(&self, on: bool) {
        self.set_state(LedState {
            on,
            ..self.state.get()
        });
        debug!("OnOff state set to: {}", on);
    }

    fn start_up_on_off(&self) -> Nullable<on_off::StartUpOnOffEnum> {
        match self.start_up_on_off.get() {
            Some(value) => Nullable::some(value),
            None => Nullable::none(),
        }
    }

    fn set_start_up_on_off(&self, value: Nullable<on_off::StartUpOnOffEnum>) -> Result<(), Error> {
        self.start_up_on_off.set(value.into_option());
        Ok(())
    }

    async fn handle_off_with_effect(&self, _effect: on_off::EffectVariantEnum) {
        // no effect
    }

    async fn run<F: Fn(on_off::OutOfBandMessage)>(&self, notify: F) {
        let mut button = self.button.lock().await;

        loop {
            button.wait_for_low().await;

            Timer::after_millis(50).await; // Debounce
            if !button.is_low() {
                continue;
            }

            let now = Instant::now();

            loop {
                button.wait_for_high().await;

                Timer::after_millis(50).await; // Debounce
                if button.is_high() {
                    break;
                }
            }

            let elapsed = Instant::now() - now;

            if elapsed.as_secs() >= 10 {
                warn!("Factory reset button held for 10 seconds, initiating factory reset");
                self.factory_reset_signal.signal(());
            } else {
                info!("Button pressed");
                notify(if self.on_off() {
                    on_off::OutOfBandMessage::Off
                } else {
                    on_off::OutOfBandMessage::On
                });
            }
        }
    }
}

impl<'a, A: adc::Instance + adc::RegisterAccess + 'a, AC: AdcChannel + AnalogPin + 'a>
    LevelControlHooks for Led<'a, A, AC>
{
    const MIN_LEVEL: u8 = 1;

    const MAX_LEVEL: u8 = 254;

    const FASTEST_RATE: u8 = 50;

    const CLUSTER: Cluster<'static> = level_control::FULL_CLUSTER
        .with_features(
            level_control::Feature::LIGHTING.bits() | level_control::Feature::ON_OFF.bits(),
        )
        .with_attrs(with!(
            required;
            level_control::AttributeId::CurrentLevel
            | level_control::AttributeId::RemainingTime
            | level_control::AttributeId::MinLevel
            | level_control::AttributeId::MaxLevel
            | level_control::AttributeId::OnOffTransitionTime
            | level_control::AttributeId::OnLevel
            | level_control::AttributeId::OnTransitionTime
            | level_control::AttributeId::OffTransitionTime
            | level_control::AttributeId::DefaultMoveRate
            | level_control::AttributeId::Options
            | level_control::AttributeId::StartUpCurrentLevel
        ))
        .with_cmds(with!(
            level_control::CommandId::MoveToLevel
                | level_control::CommandId::Move
                | level_control::CommandId::Step
                | level_control::CommandId::Stop
                | level_control::CommandId::MoveToLevelWithOnOff
                | level_control::CommandId::MoveWithOnOff
                | level_control::CommandId::StepWithOnOff
                | level_control::CommandId::StopWithOnOff
        ));

    fn set_device_level(&self, level: u8) -> Result<Option<u8>, ()> {
        debug!("LedHandler::set_device_level: level {}", level);
        self.set_state(LedState {
            level,
            ..self.state.get()
        });

        Ok(Some(level))
    }

    fn current_level(&self) -> Option<u8> {
        Some(self.state.get().level)
    }

    fn set_current_level(&self, level: Option<u8>) {
        debug!("LedHandler::set_current_level: level {:?}", level);
        self.set_state(LedState {
            level: level.unwrap_or(0),
            ..self.state.get()
        });
    }

    fn start_up_current_level(&self) -> Result<Option<u8>, Error> {
        Ok(self.startup_current_level.get())
    }

    fn set_start_up_current_level(&self, value: Option<u8>) -> Result<(), Error> {
        self.startup_current_level.set(value);
        Ok(())
    }

    async fn run<F: Fn(level_control::OutOfBandMessage)>(&self, notify: F) {
        let mut adc = self.adc.lock().await;

        if let Some((adc, pin)) = adc.as_mut() {
            // The min and max values measured by the variable resistor. Obtained empirically.
            let min: u32 = 2300;
            let max: u32 = 4081;

            let mut ema_value: u32 = 0;
            // Alpha = 0.2 means 20% new value, 80% old value (adjustable)
            let alpha_num = 2; // numerator
            let alpha_den = 10; // denominator (alpha = 0.2)

            let mut old_value = 0;

            loop {
                let val = adc.read_oneshot(pin).await;

                // Exponential moving average calculation
                ema_value =
                    ((alpha_num * val as u32) + ((alpha_den - alpha_num) * ema_value)) / alpha_den;

                // map the measured value to a level value
                let value = ema_value
                    .saturating_sub(min)
                    .mul(Self::MAX_LEVEL as u32 - Self::MIN_LEVEL as u32)
                    .div_euclid(max - min)
                    .add(Self::MIN_LEVEL as u32)
                    .max(Self::MIN_LEVEL as u32)
                    .min(Self::MAX_LEVEL as u32);

                if value != old_value {
                    // Avoids small changes switching on the light.
                    if value.abs_diff(old_value) < 5 && !self.on_off() {
                        Timer::after_millis(50).await;
                        continue;
                    }

                    old_value = value;

                    debug!(
                        "Measured_val: {} | ema_val: {} | level: {}",
                        val, ema_value, value
                    );

                    notify(level_control::OutOfBandMessage::MoveToLevel {
                        with_on_off: true,
                        level: value as u8,
                        transition_time: Some(0),
                        options_mask: OptionsBitmap::default(),
                        options_override: OptionsBitmap::default(),
                    })
                }

                Timer::after_millis(50).await;
            }
        } else {
            core::future::pending::<()>().await;
        }
    }
}

impl<'a, A: adc::Instance + adc::RegisterAccess + 'a, AC: AdcChannel + AnalogPin + 'a>
    ColorControlHooks for Led<'a, A, AC>
{
    async fn color(&self) -> (u16, u16) {
        let state = self.state.get();

        (state.x, state.y)
    }

    async fn set_color(&self, x: u16, y: u16, execute_if_off: bool) -> Result<bool, Error> {
        if !self.state.get().on || execute_if_off {
            self.set_state(LedState {
                x,
                y,
                ..self.state.get()
            });

            Ok(true)
        } else {
            info!("Not setting color because LED is off");
            Ok(false)
        }
    }

    async fn run(&self) {
        let mut led = self.led.lock().await;

        loop {
            let state = self.state_signal.wait().await;

            info!("Applying LED state: {:?}", state);

            let x_f32 = state.x as f32 / 65536.0;
            let y_f32 = state.y as f32 / 65536.0;

            let yxy: Yxy<D65, f32> = Yxy::new(x_f32, y_f32, 1.0);

            let srgb: Srgb<f32> = Srgb::from_color(yxy);

            let r = (srgb.red * 255.0) as u8;
            let g = (srgb.green * 255.0) as u8;
            let b = (srgb.blue * 255.0) as u8;

            led.write(brightness(
                gamma(once(smart_leds::RGB {
                    // LED is wired in GRB order on the WaveShare Zero board
                    r: g,
                    g: r,
                    b,
                })),
                if state.on { state.level } else { 0 },
            ))
            .await
            .unwrap();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
struct LedState {
    on: bool,
    level: u8,
    x: u16,
    y: u16,
}
