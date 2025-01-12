#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    attr, to_binary, Addr, Binary, Deps, DepsMut, Env, MessageInfo, Order, Response, StdResult,
    SubMsg, Uint64,
};
use cw2::set_contract_version;
use cw4::{Member, MemberListResponse, MemberResponse};
use cw4::{MemberChangedHookMsg, MemberDiff, TotalWeightResponse};
use cw_storage_plus::Bound;
use cw_utils::maybe_addr;

use crate::error::ContractError;
use crate::msg::{ExecuteMsg, InstantiateMsg, QueryMsg};
use crate::state::{ADMIN, HOOKS, IDS, MAX_WEIGHT, MEMBERS, MIN_WEIGHT, TOTAL};

// version info for migration info
const CONTRACT_NAME: &str = "crates.io:cw4-group";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

// Note, you can use StdResult in some functions where you do not
// make use of the custom errors
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    create(
        deps,
        msg.admin,
        msg.members,
        msg.min_weight,
        msg.max_weight,
        env.block.height,
    )?;
    Ok(Response::default())
}

// create is the instantiation logic with set_contract_version removed so it can more
// easily be imported in other contracts
pub fn create(
    mut deps: DepsMut,
    admin: Option<String>,
    members: Vec<Member>,
    min_weight: u64,
    max_weight: u64,
    height: u64,
) -> Result<(), ContractError> {
    MAX_WEIGHT.save(deps.storage, &max_weight)?;
    MIN_WEIGHT.save(deps.storage, &min_weight)?;

    let admin_addr = admin
        .map(|admin| deps.api.addr_validate(&admin))
        .transpose()?;
    ADMIN.set(deps.branch(), admin_addr)?;

    let mut total = Uint64::zero();
    for member in members.into_iter() {
        let member_weight = Uint64::from(member.weight);
        total = total.checked_add(member_weight)?;
        let member_addr = deps.api.addr_validate(&member.addr)?;
        MEMBERS.save(deps.storage, &member_addr, &member_weight.u64(), height)?;
        IDS.save(deps.storage, &member_addr, &member.identity)?;
    }
    TOTAL.save(deps.storage, &total.u64(), height)?;
    assert_weights(deps.as_ref())?;
    Ok(())
}

// And declare a custom Error variant for the ones where you will want to make use of it
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    let api = deps.api;
    match msg {
        ExecuteMsg::UpdateAdmin { admin } => Ok(ADMIN.execute_update_admin(
            deps,
            info,
            admin.map(|admin| api.addr_validate(&admin)).transpose()?,
        )?),
        ExecuteMsg::UpdateMembers { add, remove } => {
            execute_update_members(deps, env, info, add, remove)
        }
        ExecuteMsg::AddHook { addr } => {
            Ok(HOOKS.execute_add_hook(&ADMIN, deps, info, api.addr_validate(&addr)?)?)
        }
        ExecuteMsg::RemoveHook { addr } => {
            Ok(HOOKS.execute_remove_hook(&ADMIN, deps, info, api.addr_validate(&addr)?)?)
        }
    }
}

pub fn execute_update_members(
    mut deps: DepsMut,
    env: Env,
    info: MessageInfo,
    add: Vec<Member>,
    remove: Vec<String>,
) -> Result<Response, ContractError> {
    let attributes = vec![
        attr("action", "update_members"),
        attr("added", add.len().to_string()),
        attr("removed", remove.len().to_string()),
        attr("sender", &info.sender),
    ];

    // make the local update
    let diff = update_members(deps.branch(), env.block.height, info.sender, add, remove)?;
    // call all registered hooks
    let messages = HOOKS.prepare_hooks(deps.storage, |h| {
        diff.clone().into_cosmos_msg(h).map(SubMsg::new)
    })?;
    assert_weights(deps.as_ref())?;
    Ok(Response::new()
        .add_submessages(messages)
        .add_attributes(attributes))
}

// the logic from execute_update_members extracted for easier import
pub fn update_members(
    deps: DepsMut,
    height: u64,
    sender: Addr,
    to_add: Vec<Member>,
    to_remove: Vec<String>,
) -> Result<MemberChangedHookMsg, ContractError> {
    ADMIN.assert_admin(deps.as_ref(), &sender)?;

    let mut total = Uint64::from(TOTAL.load(deps.storage)?);
    let mut diffs: Vec<MemberDiff> = vec![];

    // add all new members and update total
    for add in to_add.into_iter() {
        let add_addr = deps.api.addr_validate(&add.addr)?;
        IDS.save(deps.storage, &add_addr, &add.identity)?;
        MEMBERS.update(deps.storage, &add_addr, height, |old| -> StdResult<_> {
            total = total.checked_sub(Uint64::from(old.clone().unwrap_or_default()))?;
            total = total.checked_add(Uint64::from(add.weight))?;
            diffs.push(MemberDiff::new(add.addr, old, Some(add.weight)));
            Ok(add.weight)
        })?;
    }

    for remove in to_remove.into_iter() {
        let remove_addr = deps.api.addr_validate(&remove)?;
        let old = MEMBERS.may_load(deps.storage, &remove_addr)?;
        // Only process this if they were actually in the list before
        if let Some(weight) = old {
            diffs.push(MemberDiff::new(remove, Some(weight), None));
            total = total.checked_sub(Uint64::from(weight))?;
            MEMBERS.remove(deps.storage, &remove_addr, height)?;
        }
    }

    TOTAL.save(deps.storage, &total.u64(), height)?;
    assert_weights(deps.as_ref())?;
    Ok(MemberChangedHookMsg { diffs })
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Member {
            addr,
            at_height: height,
        } => to_binary(&query_member(deps, addr, height)?),
        QueryMsg::ListMembers { start_after, limit } => {
            to_binary(&query_list_members(deps, start_after, limit)?)
        }
        QueryMsg::TotalWeight { at_height: height } => {
            to_binary(&query_total_weight(deps, height)?)
        }
        QueryMsg::Admin {} => to_binary(&ADMIN.query_admin(deps)?),
        QueryMsg::Hooks {} => to_binary(&HOOKS.query_hooks(deps)?),
    }
}

pub fn query_total_weight(deps: Deps, height: Option<u64>) -> StdResult<TotalWeightResponse> {
    let weight = match height {
        Some(h) => TOTAL.may_load_at_height(deps.storage, h),
        None => TOTAL.may_load(deps.storage),
    }?
    .unwrap_or_default();
    Ok(TotalWeightResponse { weight })
}

pub fn query_member(deps: Deps, addr: String, height: Option<u64>) -> StdResult<MemberResponse> {
    let addr = deps.api.addr_validate(&addr)?;
    let res = match height {
        Some(h) => MEMBERS.may_load_at_height(deps.storage, &addr, h),
        None => MEMBERS.may_load(deps.storage, &addr),
    }?;
    match res {
        Some(weight) => Ok(MemberResponse {
            weight: Some(weight),
            identity: IDS.load(deps.storage, &addr).ok(),
        }),
        None => Ok(MemberResponse {
            weight: None,
            identity: None,
        }),
    }
}

// settings for pagination
const MAX_LIMIT: u32 = 30;
const DEFAULT_LIMIT: u32 = 10;

pub fn query_list_members(
    deps: Deps,
    start_after: Option<String>,
    limit: Option<u32>,
) -> StdResult<MemberListResponse> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
    let addr = maybe_addr(deps.api, start_after)?;
    let start = addr.as_ref().map(Bound::exclusive);

    let members = MEMBERS
        .range(deps.storage, start, None, Order::Ascending)
        .take(limit)
        .map(|item| {
            item.map(|(addr, weight)| Member {
                addr: addr.to_string(),
                weight,
                // This should always have been set
                identity: IDS.load(deps.storage, &addr).unwrap(),
            })
        })
        .collect::<StdResult<Vec<Member>>>()?;

    Ok(MemberListResponse { members })
}

fn assert_weights(deps: Deps) -> Result<(), ContractError> {
    let min = MIN_WEIGHT.load(deps.storage)?;
    let max = MAX_WEIGHT.load(deps.storage)?;
    let total = MEMBERS
        .range(deps.storage, None, None, Order::Ascending)
        .fold(0u64, |t, m| match m {
            Ok((_, w)) => t + w,
            _ => t,
        });
    if total > max {
        return Err(ContractError::MaxWeightExceeded {});
    };

    if total < min {
        return Err(ContractError::MinWeightNotMet {});
    };

    Ok(())
}
