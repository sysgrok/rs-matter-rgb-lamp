#![allow(async_fn_in_trait)]

use core::cell::Cell;

use rs_matter_embassy::matter::dm::HandlerContext;
use rs_matter_embassy::matter::dm::clusters::level_control::OptionsBitmap;
use rs_matter_embassy::matter::dm::{Cluster, Dataver, InvokeContext, ReadContext, WriteContext};
use rs_matter_embassy::matter::error::{Error, ErrorCode};
use rs_matter_embassy::matter::tlv::Nullable;
use rs_matter_embassy::matter::{import, with};

use crate::logging::{debug, info, warn};

pub use color_control::*;

import!(ColorControl);

// TODO: Future
// #[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
// #[cfg_attr(feature = "defmt", defmt::Format)]
// pub enum Mode {
//     Solid,
//     Pulse { pulse_duration: u8 },
//     ColourPulsing { pulse_duration: u8 },
//     ColourChanging { speed: u8 },
// }

pub struct ColorControlHandler<T: ColorControlHooks> {
    dataver: Dataver,
    hooks: T,
    options: Cell<OptionsBitmap>,
}

impl<T: ColorControlHooks> ColorControlHandler<T> {
    pub fn new(dataver: Dataver, hooks: T) -> Self {
        Self {
            dataver,
            hooks,
            options: Cell::new(OptionsBitmap::empty()),
        }
    }

    /// Adapt the handler instance to the generic `rs-matter` `Handler` trait
    pub const fn adapt(self) -> HandlerAsyncAdaptor<Self> {
        HandlerAsyncAdaptor(self)
    }

    fn execute_if_off(&self, options_mask: u8, options_override: u8) -> bool {
        let mask = OptionsBitmap::from_bits_truncate(options_mask);

        if mask.contains(OptionsBitmap::EXECUTE_IF_OFF) {
            OptionsBitmap::from_bits_truncate(options_override)
                .contains(OptionsBitmap::EXECUTE_IF_OFF)
        } else {
            self.options.get().contains(OptionsBitmap::EXECUTE_IF_OFF)
        }
    }
}

impl<T: ColorControlHooks> ClusterAsyncHandler for ColorControlHandler<T> {
    #[doc = "The cluster-metadata corresponding to this handler trait."]
    const CLUSTER: Cluster<'static> = FULL_CLUSTER
        .with_revision(7)
        .with_features(Feature::XY.bits())
        .with_attrs(with!(
            required;
            AttributeId::CurrentX
            | AttributeId::CurrentY
            | AttributeId::ColorMode
            | AttributeId::Options
            | AttributeId::EnhancedColorMode
            | AttributeId::ColorCapabilities
            | AttributeId::NumberOfPrimaries
        ))
        .with_cmds(with!(
            CommandId::MoveToColor
                | CommandId::MoveColor
                | CommandId::StepColor
                | CommandId::StopMoveStep
        ));

    fn dataver(&self) -> u32 {
        self.dataver.get()
    }

    fn dataver_changed(&self) {
        self.dataver.changed();
    }

    async fn current_x(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        debug!("ColorControl: Called current_x()");
        Ok(self.hooks.color().await.0)
    }

    async fn current_y(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        debug!("ColorControl: Called current_y()");
        Ok(self.hooks.color().await.1)
    }

    async fn color_mode(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        debug!("ColorControl: Called color_mode()");
        Ok(ColorMode::CurrentXAndCurrentY as _)
    }

    async fn options(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        debug!("ColorControl: Called options()");
        Ok(self.options.get().bits())
    }

    async fn number_of_primaries(&self, _ctx: impl ReadContext) -> Result<Nullable<u8>, Error> {
        debug!("ColorControl: Called number_of_primaries()");
        Ok(Nullable::some(0))
    }

    async fn enhanced_color_mode(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        debug!("ColorControl: Called enhanced_color_mode()");
        Ok(1) // todo needs fixing when enhanced color mode bitmap is included
    }

    async fn color_capabilities(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        debug!("ColorControl: Called color_capabilities()");
        Ok(ColorCapabilities::XY_ATTRIBUTES_SUPPORTED.bits())
    }

    async fn set_options(&self, ctx: impl WriteContext, value: u8) -> Result<(), Error> {
        info!("ColorControl: Called set_options()");
        if self.options.get() != OptionsBitmap::from_bits_truncate(value) {
            self.options.set(OptionsBitmap::from_bits_truncate(value));
            self.dataver_changed();
            ctx.notify_changed();
        }

        Ok(())
    }

    async fn handle_move_to_hue(
        &self,
        _ctx: impl InvokeContext,
        _request: MoveToHueRequest<'_>,
    ) -> Result<(), Error> {
        info!("ColorControl: Called handle_move_to_hue()");
        Err(ErrorCode::InvalidCommand.into())
    }

    async fn handle_move_hue(
        &self,
        _ctx: impl InvokeContext,
        _request: MoveHueRequest<'_>,
    ) -> Result<(), Error> {
        info!("ColorControl: Called handle_move_hue()");
        Err(ErrorCode::InvalidCommand.into())
    }

    async fn handle_step_hue(
        &self,
        _ctx: impl InvokeContext,
        _request: StepHueRequest<'_>,
    ) -> Result<(), Error> {
        info!("ColorControl: Called handle_step_hue()");
        Err(ErrorCode::InvalidCommand.into())
    }

    async fn handle_move_to_saturation(
        &self,
        _ctx: impl InvokeContext,
        _request: MoveToSaturationRequest<'_>,
    ) -> Result<(), Error> {
        info!("ColorControl: Called handle_move_to_saturation()");
        Err(ErrorCode::InvalidCommand.into())
    }

    async fn handle_move_saturation(
        &self,
        _ctx: impl InvokeContext,
        _request: MoveSaturationRequest<'_>,
    ) -> Result<(), Error> {
        info!("ColorControl: Called handle_move_saturation()");
        Err(ErrorCode::InvalidCommand.into())
    }

    async fn handle_step_saturation(
        &self,
        _ctx: impl InvokeContext,
        _request: StepSaturationRequest<'_>,
    ) -> Result<(), Error> {
        info!("ColorControl: Called handle_step_saturation()");
        Err(ErrorCode::InvalidCommand.into())
    }

    async fn handle_move_to_hue_and_saturation(
        &self,
        _ctx: impl InvokeContext,
        _request: MoveToHueAndSaturationRequest<'_>,
    ) -> Result<(), Error> {
        info!("ColorControl: Called handle_move_to_hue_and_saturation()");
        Err(ErrorCode::InvalidCommand.into())
    }

    async fn handle_move_to_color(
        &self,
        ctx: impl InvokeContext,
        request: MoveToColorRequest<'_>,
    ) -> Result<(), Error> {
        info!("ColorControl: Called handle_move_to_color()");

        if self
            .hooks
            .set_color(
                request.color_x()?,
                request.color_y()?,
                self.execute_if_off(request.options_mask()?, request.options_override()?),
            )
            .await?
        {
            self.dataver_changed();
            ctx.notify_changed();
        }

        Ok(())
    }

    async fn handle_move_color(
        &self,
        _ctx: impl InvokeContext,
        _request: MoveColorRequest<'_>,
    ) -> Result<(), Error> {
        info!("ColorControl: Called handle_move_color()");
        warn!("Not yet implemented. Doing nothing.");
        Ok(())
    }

    async fn handle_step_color(
        &self,
        _ctx: impl InvokeContext,
        _request: StepColorRequest<'_>,
    ) -> Result<(), Error> {
        info!("ColorControl: Called handle_step_color()");
        warn!("Not yet implemented. Doing nothing.");
        Ok(())
    }

    async fn handle_move_to_color_temperature(
        &self,
        _ctx: impl InvokeContext,
        _request: MoveToColorTemperatureRequest<'_>,
    ) -> Result<(), Error> {
        info!("ColorControl: Called handle_move_to_color_temperature()");
        warn!("Not yet implemented. Doing nothing.");
        Ok(())
    }

    async fn handle_enhanced_move_to_hue(
        &self,
        _ctx: impl InvokeContext,
        _request: EnhancedMoveToHueRequest<'_>,
    ) -> Result<(), Error> {
        info!("ColorControl: Called handle_enhanced_move_to_hue()");
        Err(ErrorCode::InvalidCommand.into())
    }

    async fn handle_enhanced_move_hue(
        &self,
        _ctx: impl InvokeContext,
        _request: EnhancedMoveHueRequest<'_>,
    ) -> Result<(), Error> {
        info!("ColorControl: Called handle_enhanced_move_hue()");
        Err(ErrorCode::InvalidCommand.into())
    }

    async fn handle_enhanced_step_hue(
        &self,
        _ctx: impl InvokeContext,
        _request: EnhancedStepHueRequest<'_>,
    ) -> Result<(), Error> {
        info!("ColorControl: Called handle_enhanced_step_hue()");
        Err(ErrorCode::InvalidCommand.into())
    }

    async fn handle_enhanced_move_to_hue_and_saturation(
        &self,
        _ctx: impl InvokeContext,
        _request: EnhancedMoveToHueAndSaturationRequest<'_>,
    ) -> Result<(), Error> {
        info!("ColorControl: Called handle_enhanced_move_to_hue_and_saturation()");
        Err(ErrorCode::InvalidCommand.into())
    }

    async fn handle_color_loop_set(
        &self,
        _ctx: impl InvokeContext,
        _request: ColorLoopSetRequest<'_>,
    ) -> Result<(), Error> {
        info!("ColorControl: Called handle_color_loop_set()");
        Err(ErrorCode::InvalidCommand.into())
    }

    async fn handle_stop_move_step(
        &self,
        _ctx: impl InvokeContext,
        _request: StopMoveStepRequest<'_>,
    ) -> Result<(), Error> {
        info!("ColorControl: Called handle_stop_move_step()");
        warn!("Not yet implemented. Doing nothing.");
        Ok(())
    }

    async fn handle_move_color_temperature(
        &self,
        _ctx: impl InvokeContext,
        _request: MoveColorTemperatureRequest<'_>,
    ) -> Result<(), Error> {
        info!("ColorControl: Called handle_move_color_temperature()");
        warn!("Not yet implemented. Doing nothing.");
        Ok(())
    }

    async fn handle_step_color_temperature(
        &self,
        _ctx: impl InvokeContext,
        _request: StepColorTemperatureRequest<'_>,
    ) -> Result<(), Error> {
        info!("ColorControl: Called handle_step_color_temperature()");
        warn!("Not yet implemented. Doing nothing.");
        Ok(())
    }

    async fn run(&self, _ctx: impl HandlerContext) -> Result<(), Error> {
        self.hooks.run().await;

        Ok(())
    }
}

pub trait ColorControlHooks {
    async fn color(&self) -> (u16, u16);

    // todo add the transition time
    async fn set_color(&self, x: u16, y: u16, execute_if_off: bool) -> Result<bool, Error>;

    async fn run(&self) {
        core::future::pending().await
    }
}

impl<T> ColorControlHooks for &T
where
    T: ColorControlHooks,
{
    fn color(&self) -> impl Future<Output = (u16, u16)> {
        (*self).color()
    }

    fn set_color(
        &self,
        x: u16,
        y: u16,
        execute_if_off: bool,
    ) -> impl Future<Output = Result<bool, Error>> {
        (*self).set_color(x, y, execute_if_off)
    }

    fn run(&self) -> impl Future<Output = ()> {
        (*self).run()
    }
}
