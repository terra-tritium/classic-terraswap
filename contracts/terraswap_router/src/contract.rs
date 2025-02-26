#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;

use cosmwasm_std::{
    from_binary, to_binary, Addr, Api, Binary, Coin, CosmosMsg, Deps, DepsMut, Env, MessageInfo,
    QueryRequest, Response, StdError, StdResult, Uint128, WasmMsg, WasmQuery,
};
use cw2::set_contract_version;

use crate::operations::execute_swap_operation;
use crate::querier::{compute_reverse_tax, compute_tax};
use crate::state::{Config, CONFIG};

use classic_bindings::{SwapResponse, TerraMsg, TerraQuerier, TerraQuery};

use classic_terraswap::asset::{Asset, AssetInfo, PairInfo};
use classic_terraswap::pair::{QueryMsg as PairQueryMsg, SimulationResponse};
use classic_terraswap::querier::{query_pair_info, reverse_simulate};
use classic_terraswap::router::{
    ConfigResponse, Cw20HookMsg, ExecuteMsg, InstantiateMsg, MigrateMsg, QueryMsg,
    SimulateSwapOperationsResponse, SwapOperation,
};
use classic_terraswap::util::assert_deadline;
use cw20::Cw20ReceiveMsg;
use std::collections::HashMap;

// version info for migration info
const CONTRACT_NAME: &str = "crates.io:terraswap-router";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut<TerraQuery>,
    _env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> StdResult<Response<TerraMsg>> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    CONFIG.save(
        deps.storage,
        &Config {
            terraswap_factory: deps.api.addr_canonicalize(&msg.terraswap_factory)?,
            loop_factory: deps.api.addr_canonicalize(&msg.loop_factory)?,
            astroport_factory: deps.api.addr_canonicalize(&msg.astroport_factory)?,
        },
    )?;

    Ok(Response::default())
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut<TerraQuery>,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> StdResult<Response<TerraMsg>> {
    match msg {
        ExecuteMsg::Receive(msg) => receive_cw20(deps, env, info, msg),
        ExecuteMsg::ExecuteSwapOperations {
            operations,
            minimum_receive,
            to,
            deadline,
        } => {
            let api = deps.api;
            execute_swap_operations(
                deps,
                env,
                info.sender,
                operations,
                minimum_receive,
                optional_addr_validate(api, to)?,
                deadline,
            )
        }
        ExecuteMsg::ExecuteSwapOperation {
            operation,
            to,
            deadline,
        } => {
            let api = deps.api;
            execute_swap_operation(
                deps,
                env,
                info,
                operation,
                optional_addr_validate(api, to)?.map(|v| v.to_string()),
                deadline,
            )
        }
        ExecuteMsg::AssertMinimumReceive {
            asset_info,
            prev_balance,
            minimum_receive,
            receiver,
        } => assert_minimum_receive(
            deps.as_ref(),
            asset_info,
            prev_balance,
            minimum_receive,
            deps.api.addr_validate(&receiver)?,
        ),
    }
}

fn optional_addr_validate(api: &dyn Api, addr: Option<String>) -> StdResult<Option<Addr>> {
    let addr = if let Some(addr) = addr {
        Some(api.addr_validate(&addr)?)
    } else {
        None
    };

    Ok(addr)
}

pub fn receive_cw20(
    deps: DepsMut<TerraQuery>,
    env: Env,
    _info: MessageInfo,
    cw20_msg: Cw20ReceiveMsg,
) -> StdResult<Response<TerraMsg>> {
    let sender = deps.api.addr_validate(&cw20_msg.sender)?;
    match from_binary(&cw20_msg.msg)? {
        Cw20HookMsg::ExecuteSwapOperations {
            operations,
            minimum_receive,
            to,
            deadline,
        } => {
            let api = deps.api;
            execute_swap_operations(
                deps,
                env,
                sender,
                operations,
                minimum_receive,
                optional_addr_validate(api, to)?,
                deadline,
            )
        }
    }
}

pub fn execute_swap_operations(
    deps: DepsMut<TerraQuery>,
    env: Env,
    sender: Addr,
    operations: Vec<SwapOperation>,
    minimum_receive: Option<Uint128>,
    to: Option<Addr>,
    deadline: Option<u64>,
) -> StdResult<Response<TerraMsg>> {
    assert_deadline(env.block.time.seconds(), deadline)?;
    let operations_len = operations.len();
    if operations_len == 0 {
        return Err(StdError::generic_err("must provide operations"));
    }

    // Assert the operations are properly set
    assert_operations(&operations)?;

    let to = if let Some(to) = to { to } else { sender };
    let target_asset_info = operations.last().unwrap().get_target_asset_info();

    let mut operation_index = 0;
    let mut messages: Vec<CosmosMsg<TerraMsg>> = operations
        .into_iter()
        .map(|op| {
            operation_index += 1;
            Ok(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: env.contract.address.to_string(),
                funds: vec![],
                msg: to_binary(&ExecuteMsg::ExecuteSwapOperation {
                    operation: op,
                    to: if operation_index == operations_len {
                        Some(to.to_string())
                    } else {
                        None
                    },
                    deadline: None,
                })?,
            }))
        })
        .collect::<StdResult<Vec<CosmosMsg<TerraMsg>>>>()?;

    // Execute minimum amount assertion
    if let Some(minimum_receive) = minimum_receive {
        let receiver_balance = target_asset_info.query_pool(&deps.querier, deps.api, to.clone())?;

        messages.push(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: env.contract.address.to_string(),
            funds: vec![],
            msg: to_binary(&ExecuteMsg::AssertMinimumReceive {
                asset_info: target_asset_info,
                prev_balance: receiver_balance,
                minimum_receive,
                receiver: to.to_string(),
            })?,
        }))
    }

    Ok(Response::new().add_messages(messages))
}

fn assert_minimum_receive(
    deps: Deps<TerraQuery>,
    asset_info: AssetInfo,
    prev_balance: Uint128,
    minium_receive: Uint128,
    receiver: Addr,
) -> StdResult<Response<TerraMsg>> {
    let receiver_balance = asset_info.query_pool(&deps.querier, deps.api, receiver)?;
    let swap_amount = receiver_balance.checked_sub(prev_balance)?;

    if swap_amount < minium_receive {
        return Err(StdError::generic_err(format!(
            "assertion failed; minimum receive amount: {}, swap amount: {}",
            minium_receive, swap_amount
        )));
    }

    Ok(Response::default())
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps<TerraQuery>, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => to_binary(&query_config(deps)?),
        QueryMsg::SimulateSwapOperations {
            offer_amount,
            operations,
        } => to_binary(&simulate_swap_operations(deps, offer_amount, operations)?),
        QueryMsg::ReverseSimulateSwapOperations {
            ask_amount,
            operations,
        } => to_binary(&reverse_simulate_swap_operations(
            deps, ask_amount, operations,
        )?),
    }
}

pub fn query_config(deps: Deps<TerraQuery>) -> StdResult<ConfigResponse> {
    let state = CONFIG.load(deps.storage)?;
    let resp = ConfigResponse {
        terraswap_factory: deps
            .api
            .addr_humanize(&state.terraswap_factory)?
            .to_string(),
        loop_factory: deps.api.addr_humanize(&state.loop_factory)?.to_string(),
        astroport_factory: deps
            .api
            .addr_humanize(&state.astroport_factory)?
            .to_string(),
    };

    Ok(resp)
}

fn simulate_swap_operations(
    deps: Deps<TerraQuery>,
    offer_amount: Uint128,
    operations: Vec<SwapOperation>,
) -> StdResult<SimulateSwapOperationsResponse> {
    let config: Config = CONFIG.load(deps.storage)?;
    let terra_querier = TerraQuerier::new(&deps.querier);

    let operations_len = operations.len();
    if operations_len == 0 {
        return Err(StdError::generic_err("must provide operations"));
    }

    let mut operation_index = 0;
    let mut offer_amount = offer_amount;
    for operation in operations.into_iter() {
        operation_index += 1;

        offer_amount = match operation {
            SwapOperation::NativeSwap {
                offer_denom,
                ask_denom,
            } => {
                // Deduct tax before query simulation
                // because last swap is swap_send
                if operation_index == operations_len {
                    offer_amount = offer_amount.checked_sub(compute_tax(
                        &deps.querier,
                        offer_amount,
                        offer_denom.clone(),
                    )?)?;
                }

                let res: SwapResponse = terra_querier.query_swap(
                    Coin {
                        denom: offer_denom,
                        amount: offer_amount,
                    },
                    ask_denom,
                )?;

                res.receive.amount
            }
            SwapOperation::TerraSwap {
                offer_asset_info,
                ask_asset_info,
            } => {
                let terraswap_factory = deps.api.addr_humanize(&config.terraswap_factory)?;
                simulate_return_amount(
                    deps,
                    terraswap_factory,
                    offer_amount,
                    offer_asset_info,
                    ask_asset_info,
                )
                .unwrap()
            }
            SwapOperation::Loop {
                offer_asset_info,
                ask_asset_info,
            } => {
                let loop_factory = deps.api.addr_humanize(&config.loop_factory)?;
                simulate_return_amount(
                    deps,
                    loop_factory,
                    offer_amount,
                    offer_asset_info,
                    ask_asset_info,
                )
                .unwrap()
            }
            SwapOperation::Astroport {
                offer_asset_info,
                ask_asset_info,
            } => {
                let astroport_factory = deps.api.addr_humanize(&config.astroport_factory)?;
                simulate_return_amount(
                    deps,
                    astroport_factory,
                    offer_amount,
                    offer_asset_info,
                    ask_asset_info,
                )
                .unwrap()
            }
        }
    }

    Ok(SimulateSwapOperationsResponse {
        amount: offer_amount,
    })
}

fn reverse_simulate_swap_operations(
    deps: Deps<TerraQuery>,
    ask_amount: Uint128,
    operations: Vec<SwapOperation>,
) -> StdResult<SimulateSwapOperationsResponse> {
    let config: Config = CONFIG.load(deps.storage)?;

    let operations_len = operations.len();
    if operations_len == 0 {
        return Err(StdError::generic_err("must provide operations"));
    }

    let mut ask_amount = ask_amount;
    for operation in operations.into_iter().rev() {
        ask_amount = match operation {
            SwapOperation::NativeSwap {
                offer_denom: _,
                ask_denom: _,
            } => {
                return Err(StdError::generic_err(
                    "reverse simulation of native_swap is not supported yet",
                ))
            }
            SwapOperation::TerraSwap {
                offer_asset_info,
                ask_asset_info,
            } => {
                let terraswap_factory = deps.api.addr_humanize(&config.terraswap_factory)?;

                reverse_simulate_return_amount(
                    deps,
                    terraswap_factory,
                    ask_amount,
                    offer_asset_info,
                    ask_asset_info,
                )
                .unwrap()
            }
            SwapOperation::Loop {
                offer_asset_info,
                ask_asset_info,
            } => {
                let loop_factory = deps.api.addr_humanize(&config.loop_factory)?;

                reverse_simulate_return_amount(
                    deps,
                    loop_factory,
                    ask_amount,
                    offer_asset_info,
                    ask_asset_info,
                )
                .unwrap()
            }
            SwapOperation::Astroport {
                offer_asset_info,
                ask_asset_info,
            } => {
                let astroport_factory = deps.api.addr_humanize(&config.astroport_factory)?;

                reverse_simulate_return_amount(
                    deps,
                    astroport_factory,
                    ask_amount,
                    offer_asset_info,
                    ask_asset_info,
                )
                .unwrap()
            }
        }
    }

    Ok(SimulateSwapOperationsResponse { amount: ask_amount })
}

fn simulate_return_amount(
    deps: Deps<TerraQuery>,
    factory: Addr,
    mut offer_amount: Uint128,
    offer_asset_info: AssetInfo,
    ask_asset_info: AssetInfo,
) -> StdResult<Uint128> {
    let pair_info: PairInfo = query_pair_info(
        &deps.querier,
        factory,
        &[offer_asset_info.clone(), ask_asset_info.clone()],
    )?;

    // Deduct tax before querying simulation
    if let AssetInfo::NativeToken { denom } = offer_asset_info.clone() {
        offer_amount =
            offer_amount.checked_sub(compute_tax(&deps.querier, offer_amount, denom)?)?;
    }

    let mut res: SimulationResponse =
        deps.querier.query(&QueryRequest::Wasm(WasmQuery::Smart {
            contract_addr: pair_info.contract_addr,
            msg: to_binary(&PairQueryMsg::Simulation {
                offer_asset: Asset {
                    info: offer_asset_info,
                    amount: offer_amount,
                },
            })?,
        }))?;

    // Deduct tax after querying simulation
    if let AssetInfo::NativeToken { denom } = ask_asset_info {
        res.return_amount =
            res.return_amount
                .checked_sub(compute_tax(&deps.querier, res.return_amount, denom)?)?;
    }

    Ok(res.return_amount)
}

fn reverse_simulate_return_amount(
    deps: Deps<TerraQuery>,
    factory: Addr,
    ask_amount: Uint128,
    offer_asset_info: AssetInfo,
    ask_asset_info: AssetInfo,
) -> StdResult<Uint128> {
    let pair_info: PairInfo = query_pair_info(
        &deps.querier,
        factory,
        &[offer_asset_info.clone(), ask_asset_info.clone()],
    )?;

    let mut res = reverse_simulate(
        &deps.querier,
        Addr::unchecked(pair_info.contract_addr),
        &Asset {
            amount: ask_amount,
            info: ask_asset_info,
        },
    )?;

    // Add tax after querying simulation
    if let AssetInfo::NativeToken { denom } = offer_asset_info {
        res.offer_amount = res.offer_amount.checked_add(compute_reverse_tax(
            &deps.querier,
            res.offer_amount,
            denom,
        )?)?;
    }

    Ok(res.offer_amount)
}

fn assert_operations(operations: &[SwapOperation]) -> StdResult<()> {
    let mut ask_asset_map: HashMap<String, bool> = HashMap::new();
    for operation in operations.iter() {
        let (offer_asset, ask_asset) = match operation {
            SwapOperation::NativeSwap {
                offer_denom,
                ask_denom,
            } => (
                AssetInfo::NativeToken {
                    denom: offer_denom.clone(),
                },
                AssetInfo::NativeToken {
                    denom: ask_denom.clone(),
                },
            ),
            SwapOperation::TerraSwap {
                offer_asset_info,
                ask_asset_info,
            }
            | SwapOperation::Loop {
                offer_asset_info,
                ask_asset_info,
            }
            | SwapOperation::Astroport {
                offer_asset_info,
                ask_asset_info,
            } => (offer_asset_info.clone(), ask_asset_info.clone()),
        };

        ask_asset_map.remove(&offer_asset.to_string());
        ask_asset_map.insert(ask_asset.to_string(), true);
    }

    if ask_asset_map.keys().len() != 1 {
        return Err(StdError::generic_err(
            "invalid operations; multiple output token",
        ));
    }

    Ok(())
}

#[test]
fn test_invalid_operations() {
    // empty error
    assert!(assert_operations(&[]).is_err());

    // uluna output
    assert!(assert_operations(&vec![
        SwapOperation::NativeSwap {
            offer_denom: "uusd".to_string(),
            ask_denom: "uluna".to_string(),
        },
        SwapOperation::TerraSwap {
            offer_asset_info: AssetInfo::NativeToken {
                denom: "ukrw".to_string(),
            },
            ask_asset_info: AssetInfo::Token {
                contract_addr: "asset0001".to_string(),
            },
        },
        SwapOperation::TerraSwap {
            offer_asset_info: AssetInfo::Token {
                contract_addr: "asset0001".to_string(),
            },
            ask_asset_info: AssetInfo::NativeToken {
                denom: "uluna".to_string(),
            },
        }
    ])
    .is_ok());

    // asset0002 output
    assert!(assert_operations(&vec![
        SwapOperation::NativeSwap {
            offer_denom: "uusd".to_string(),
            ask_denom: "uluna".to_string(),
        },
        SwapOperation::TerraSwap {
            offer_asset_info: AssetInfo::NativeToken {
                denom: "ukrw".to_string(),
            },
            ask_asset_info: AssetInfo::Token {
                contract_addr: "asset0001".to_string(),
            },
        },
        SwapOperation::TerraSwap {
            offer_asset_info: AssetInfo::Token {
                contract_addr: "asset0001".to_string(),
            },
            ask_asset_info: AssetInfo::NativeToken {
                denom: "uluna".to_string(),
            },
        },
        SwapOperation::TerraSwap {
            offer_asset_info: AssetInfo::NativeToken {
                denom: "uluna".to_string(),
            },
            ask_asset_info: AssetInfo::Token {
                contract_addr: "asset0002".to_string(),
            },
        },
    ])
    .is_ok());

    // multiple output token types error
    assert!(assert_operations(&vec![
        SwapOperation::NativeSwap {
            offer_denom: "uusd".to_string(),
            ask_denom: "ukrw".to_string(),
        },
        SwapOperation::TerraSwap {
            offer_asset_info: AssetInfo::NativeToken {
                denom: "ukrw".to_string(),
            },
            ask_asset_info: AssetInfo::Token {
                contract_addr: "asset0001".to_string(),
            },
        },
        SwapOperation::TerraSwap {
            offer_asset_info: AssetInfo::Token {
                contract_addr: "asset0001".to_string(),
            },
            ask_asset_info: AssetInfo::NativeToken {
                denom: "uaud".to_string(),
            },
        },
        SwapOperation::TerraSwap {
            offer_asset_info: AssetInfo::NativeToken {
                denom: "uluna".to_string(),
            },
            ask_asset_info: AssetInfo::Token {
                contract_addr: "asset0002".to_string(),
            },
        },
    ])
    .is_err());
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(deps: DepsMut, _env: Env, _msg: MigrateMsg) -> StdResult<Response> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    Ok(Response::default())
}
