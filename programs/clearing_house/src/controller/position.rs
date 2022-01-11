use anchor_lang::prelude::*;
use borsh::{BorshDeserialize, BorshSerialize};

use crate::controller;
use crate::controller::amm::SwapDirection;
use crate::error::*;
use crate::math::casting::{cast, cast_to_i128};
use crate::math::collateral::calculate_updated_collateral;
use crate::math::pnl::calculate_pnl;
use crate::math_error;
use crate::{Market, MarketPosition, User, UserPositions};
use solana_program::msg;
use std::cell::RefMut;

#[derive(Clone, Copy, BorshSerialize, BorshDeserialize, PartialEq)]
pub enum PositionDirection {
    Long,
    Short,
}

impl Default for PositionDirection {
    // UpOnly
    fn default() -> Self {
        PositionDirection::Long
    }
}

pub fn add_new_position(
    user_positions: &mut RefMut<UserPositions>,
    market_index: u64,
) -> ClearingHouseResult<usize> {
    let new_position_index = user_positions
        .positions
        .iter()
        .position(|market_position| market_position.is_available())
        .ok_or(ErrorCode::MaxNumberOfPositions)?;

    let new_market_position = MarketPosition {
        market_index,
        base_asset_amount: 0,
        quote_asset_amount: 0,
        last_cumulative_funding_rate: 0,
        last_cumulative_repeg_rebate: 0,
        last_funding_rate_ts: 0,
        open_orders: 0,
        padding0: 0,
        padding1: 0,
        padding2: 0,
        padding3: 0,
        padding4: 0,
        padding5: 0,
        padding6: 0,
    };

    user_positions.positions[new_position_index] = new_market_position;

    return Ok(new_position_index);
}

pub fn get_position_index(
    user_positions: &mut RefMut<UserPositions>,
    market_index: u64,
) -> ClearingHouseResult<usize> {
    let position_index = user_positions
        .positions
        .iter_mut()
        .position(|market_position| market_position.is_for(market_index));

    match position_index {
        Some(position_index) => Ok(position_index),
        None => Err(ErrorCode::UserHasNoPositionInMarket.into()),
    }
}

pub fn increase(
    direction: PositionDirection,
    quote_asset_amount: u128,
    market: &mut Market,
    market_position: &mut MarketPosition,
    now: i64,
) -> ClearingHouseResult<i128> {
    if quote_asset_amount == 0 {
        return Ok(0);
    }

    // Update funding rate if this is a new position
    if market_position.base_asset_amount == 0 {
        market_position.last_cumulative_funding_rate = match direction {
            PositionDirection::Long => market.amm.cumulative_funding_rate_long,
            PositionDirection::Short => market.amm.cumulative_funding_rate_short,
        };

        market.open_interest = market
            .open_interest
            .checked_add(1)
            .ok_or_else(math_error!())?;
    }

    market_position.quote_asset_amount = market_position
        .quote_asset_amount
        .checked_add(quote_asset_amount)
        .ok_or_else(math_error!())?;

    let swap_direction = match direction {
        PositionDirection::Long => SwapDirection::Add,
        PositionDirection::Short => SwapDirection::Remove,
    };

    let base_asset_acquired = controller::amm::swap_quote_asset(
        &mut market.amm,
        quote_asset_amount,
        swap_direction,
        now,
        None,
    )?;

    // update the position size on market and user
    market_position.base_asset_amount = market_position
        .base_asset_amount
        .checked_add(base_asset_acquired)
        .ok_or_else(math_error!())?;
    market.base_asset_amount = market
        .base_asset_amount
        .checked_add(base_asset_acquired)
        .ok_or_else(math_error!())?;

    if market_position.base_asset_amount > 0 {
        market.base_asset_amount_long = market
            .base_asset_amount_long
            .checked_add(base_asset_acquired)
            .ok_or_else(math_error!())?;
    } else {
        market.base_asset_amount_short = market
            .base_asset_amount_short
            .checked_add(base_asset_acquired)
            .ok_or_else(math_error!())?;
    }

    Ok(base_asset_acquired)
}

pub fn increase_with_base_asset_amount(
    direction: PositionDirection,
    base_asset_amount: u128,
    market: &mut Market,
    market_position: &mut MarketPosition,
    now: i64,
) -> ClearingHouseResult {
    if base_asset_amount == 0 {
        return Ok(());
    }

    // Update funding rate if this is a new position
    if market_position.base_asset_amount == 0 {
        market_position.last_cumulative_funding_rate = match direction {
            PositionDirection::Long => market.amm.cumulative_funding_rate_long,
            PositionDirection::Short => market.amm.cumulative_funding_rate_short,
        };

        market.open_interest = market
            .open_interest
            .checked_add(1)
            .ok_or_else(math_error!())?;
    }

    let swap_direction = match direction {
        PositionDirection::Long => SwapDirection::Remove,
        PositionDirection::Short => SwapDirection::Add,
    };

    let quote_asset_swapped =
        controller::amm::swap_base_asset(&mut market.amm, base_asset_amount, swap_direction, now)?;

    market_position.quote_asset_amount = market_position
        .quote_asset_amount
        .checked_add(quote_asset_swapped)
        .ok_or_else(math_error!())?;

    let base_asset_amount = match direction {
        PositionDirection::Long => cast_to_i128(base_asset_amount)?,
        PositionDirection::Short => -cast_to_i128(base_asset_amount)?,
    };

    market_position.base_asset_amount = market_position
        .base_asset_amount
        .checked_add(base_asset_amount)
        .ok_or_else(math_error!())?;
    market.base_asset_amount = market
        .base_asset_amount
        .checked_add(base_asset_amount)
        .ok_or_else(math_error!())?;

    if market_position.base_asset_amount > 0 {
        market.base_asset_amount_long = market
            .base_asset_amount_long
            .checked_add(base_asset_amount)
            .ok_or_else(math_error!())?;
    } else {
        market.base_asset_amount_short = market
            .base_asset_amount_short
            .checked_add(base_asset_amount)
            .ok_or_else(math_error!())?;
    }

    Ok(())
}

pub fn reduce<'info>(
    direction: PositionDirection,
    quote_asset_swap_amount: u128,
    user: &mut Account<'info, User>,
    market: &mut Market,
    market_position: &mut MarketPosition,
    now: i64,
    precomputed_mark_price: Option<u128>,
) -> ClearingHouseResult<i128> {
    let swap_direction = match direction {
        PositionDirection::Long => SwapDirection::Add,
        PositionDirection::Short => SwapDirection::Remove,
    };

    let base_asset_swapped = controller::amm::swap_quote_asset(
        &mut market.amm,
        quote_asset_swap_amount,
        swap_direction,
        now,
        precomputed_mark_price,
    )?;

    let base_asset_amount_before = market_position.base_asset_amount;
    market_position.base_asset_amount = market_position
        .base_asset_amount
        .checked_add(base_asset_swapped)
        .ok_or_else(math_error!())?;

    market.open_interest = market
        .open_interest
        .checked_sub(cast(market_position.base_asset_amount == 0)?)
        .ok_or_else(math_error!())?;
    market.base_asset_amount = market
        .base_asset_amount
        .checked_add(base_asset_swapped)
        .ok_or_else(math_error!())?;

    if market_position.base_asset_amount > 0 {
        market.base_asset_amount_long = market
            .base_asset_amount_long
            .checked_add(base_asset_swapped)
            .ok_or_else(math_error!())?;
    } else {
        market.base_asset_amount_short = market
            .base_asset_amount_short
            .checked_add(base_asset_swapped)
            .ok_or_else(math_error!())?;
    }

    let base_asset_amount_change = base_asset_amount_before
        .checked_sub(market_position.base_asset_amount)
        .ok_or_else(math_error!())?
        .abs();

    let initial_quote_asset_amount_closed = market_position
        .quote_asset_amount
        .checked_mul(base_asset_amount_change.unsigned_abs())
        .ok_or_else(math_error!())?
        .checked_div(base_asset_amount_before.unsigned_abs())
        .ok_or_else(math_error!())?;

    market_position.quote_asset_amount = market_position
        .quote_asset_amount
        .checked_sub(initial_quote_asset_amount_closed)
        .ok_or_else(math_error!())?;

    let pnl = if market_position.base_asset_amount > 0 {
        cast_to_i128(quote_asset_swap_amount)?
            .checked_sub(cast(initial_quote_asset_amount_closed)?)
            .ok_or_else(math_error!())?
    } else {
        cast_to_i128(initial_quote_asset_amount_closed)?
            .checked_sub(cast(quote_asset_swap_amount)?)
            .ok_or_else(math_error!())?
    };

    user.collateral = calculate_updated_collateral(user.collateral, pnl)?;

    Ok(base_asset_swapped)
}

pub fn reduce_with_base_asset_amount<'info>(
    direction: PositionDirection,
    base_asset_amount: u128,
    user: &mut User,
    market: &mut Market,
    market_position: &mut MarketPosition,
    now: i64,
) -> ClearingHouseResult {
    let swap_direction = match direction {
        PositionDirection::Long => SwapDirection::Remove,
        PositionDirection::Short => SwapDirection::Add,
    };

    let quote_asset_swapped =
        controller::amm::swap_base_asset(&mut market.amm, base_asset_amount, swap_direction, now)?;

    let base_asset_amount = match direction {
        PositionDirection::Long => cast_to_i128(base_asset_amount)?,
        PositionDirection::Short => -cast_to_i128(base_asset_amount)?,
    };

    let base_asset_amount_before = market_position.base_asset_amount;
    market_position.base_asset_amount = market_position
        .base_asset_amount
        .checked_add(base_asset_amount)
        .ok_or_else(math_error!())?;

    market.open_interest = market
        .open_interest
        .checked_sub(cast(market_position.base_asset_amount == 0)?)
        .ok_or_else(math_error!())?;
    market.base_asset_amount = market
        .base_asset_amount
        .checked_add(base_asset_amount)
        .ok_or_else(math_error!())?;

    if market_position.base_asset_amount > 0 {
        market.base_asset_amount_long = market
            .base_asset_amount_long
            .checked_add(base_asset_amount)
            .ok_or_else(math_error!())?;
    } else {
        market.base_asset_amount_short = market
            .base_asset_amount_short
            .checked_add(base_asset_amount)
            .ok_or_else(math_error!())?;
    }

    let base_asset_amount_change = base_asset_amount_before
        .checked_sub(market_position.base_asset_amount)
        .ok_or_else(math_error!())?
        .abs();

    let initial_quote_asset_amount_closed = market_position
        .quote_asset_amount
        .checked_mul(base_asset_amount_change.unsigned_abs())
        .ok_or_else(math_error!())?
        .checked_div(base_asset_amount_before.unsigned_abs())
        .ok_or_else(math_error!())?;

    market_position.quote_asset_amount = market_position
        .quote_asset_amount
        .checked_sub(initial_quote_asset_amount_closed)
        .ok_or_else(math_error!())?;

    let pnl = if PositionDirection::Short == direction {
        cast_to_i128(quote_asset_swapped)?
            .checked_sub(cast(initial_quote_asset_amount_closed)?)
            .ok_or_else(math_error!())?
    } else {
        cast_to_i128(initial_quote_asset_amount_closed)?
            .checked_sub(cast(quote_asset_swapped)?)
            .ok_or_else(math_error!())?
    };

    user.collateral = calculate_updated_collateral(user.collateral, pnl)?;

    Ok(())
}

pub fn close(
    user: &mut User,
    market: &mut Market,
    market_position: &mut MarketPosition,
    now: i64,
) -> ClearingHouseResult<(u128, i128)> {
    // If user has no base asset, return early
    if market_position.base_asset_amount == 0 {
        return Ok((0, 0));
    }

    let swap_direction = if market_position.base_asset_amount > 0 {
        SwapDirection::Add
    } else {
        SwapDirection::Remove
    };

    let base_asset_value = controller::amm::swap_base_asset(
        &mut market.amm,
        market_position.base_asset_amount.unsigned_abs(),
        swap_direction,
        now,
    )?;
    let pnl = calculate_pnl(
        base_asset_value,
        market_position.quote_asset_amount,
        swap_direction,
    )?;

    user.collateral = calculate_updated_collateral(user.collateral, pnl)?;
    market_position.last_cumulative_funding_rate = 0;
    market_position.last_funding_rate_ts = 0;

    market.open_interest = market
        .open_interest
        .checked_sub(1)
        .ok_or_else(math_error!())?;

    market_position.quote_asset_amount = 0;

    market.base_asset_amount = market
        .base_asset_amount
        .checked_sub(market_position.base_asset_amount)
        .ok_or_else(math_error!())?;

    if market_position.base_asset_amount > 0 {
        market.base_asset_amount_long = market
            .base_asset_amount_long
            .checked_sub(market_position.base_asset_amount)
            .ok_or_else(math_error!())?;
    } else {
        market.base_asset_amount_short = market
            .base_asset_amount_short
            .checked_sub(market_position.base_asset_amount)
            .ok_or_else(math_error!())?;
    }

    let base_asset_amount = market_position.base_asset_amount;
    market_position.base_asset_amount = 0;

    Ok((base_asset_value, base_asset_amount))
}
