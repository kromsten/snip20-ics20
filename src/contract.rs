//use asset::Contract;
use cosmwasm_std::{
    entry_point, from_binary, to_binary, Addr, Binary, CosmosMsg, Deps, DepsMut, Env, IbcMsg,
    MessageInfo, Response, StdResult,
};

use crate::amount::Snip20Coin;
use crate::error::ContractError;
use crate::ibc::Ics20Packet;
use crate::msg::{ExecuteMsg, InitMsg, QueryMsg, Snip20Data, Snip20ReceiveMsg, TransferMsg};
use secret_toolkit::snip20;

use crate::state::{increase_channel_balance, CHANNEL_INFO, CODE_HASH, CONFIG};

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    mut deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    msg: InitMsg,
) -> Result<Response, ContractError> {
    Ok(Response::default())
}

#[entry_point]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    match msg {
        ExecuteMsg::Receive(msg) => execute_receive(deps, env, info, msg),
        ExecuteMsg::RegisterTokens { tokens } => {
            let output_msgs = register_tokens(deps, env, tokens)?;

            Ok(Response::new().add_submessages(output_msgs))
        }
    }
}

pub fn execute_receive(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    wrapper: Snip20ReceiveMsg,
) -> Result<Response, ContractError> {
    let msg: TransferMsg = from_binary(&wrapper.msg.unwrap())?;
    let amount = Amount::Snip20(Snip20Coin {
        address: info.sender.to_string(),
        amount: wrapper.amount,
    });

    let api = deps.api;
    execute_transfer(
        deps,
        env,
        info,
        msg,
        amount,
        api.addr_validate(&wrapper.sender)?,
    )
}

pub fn execute_transfer(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: TransferMsg,
    amount: Amount,
    sender: Addr,
) -> Result<Response, ContractError> {
    if amount.is_empty() {
        return Err(ContractError::NoFunds {});
    }
    // ensure the requested channel is registered
    if !CHANNEL_INFO.has(deps.storage, &msg.channel) {
        return Err(ContractError::NoSuchChannel { id: msg.channel });
    }
    let config = CONFIG.load(deps.storage)?;

    // delta from user is in seconds
    let timeout_delta = match msg.timeout {
        Some(t) => t,
        None => config.default_timeout,
    };
    // timeout is in nanoseconds
    let timeout = env.block.time.plus_seconds(timeout_delta);

    // build ics20 packet
    let packet = Ics20Packet::new(
        amount.amount(),
        amount.denom(),
        sender.as_ref(),
        &msg.remote_address,
    );
    packet.validate()?;

    // Update the balance now (optimistically) like ibctransfer modules.
    // In on_packet_failure (ack with error message or a timeout), we reduce the balance appropriately.
    // This means the channel works fine if success acks are not relayed.
    increase_channel_balance(deps.storage, &msg.channel, &amount.denom(), amount.amount())?;

    // send response
    let res = Response::new()
        .add_message(msg)
        .add_messages(IbcMsg::SendPacket {
            channel_id: msg.channel,
            data: to_binary(&packet)?,
            timeout: timeout.into(),
        })
        .add_attribute("action", "transfer")
        .add_attribute("sender", &packet.sender)
        .add_attribute("receiver", &packet.receiver)
        .add_attribute("denom", &packet.denom)
        .add_attribute("amount", &packet.amount.to_string());
    Ok(res)
}

fn register_tokens(deps: DepsMut, env: Env, tokens: Vec<Snip20Data>) -> StdResult<Vec<CosmosMsg>> {
    let mut output_msgs = vec![];

    for token in tokens {
        let token_address = token.address;
        let token_code_hash = token.code_hash;

        CODE_HASH.save(
            deps.storage,
            deps.api.addr_validate(&token_address)?,
            &token_code_hash,
        )?;

        output_msgs.push(snip20::register_receive_msg(
            env.contract.code_hash.clone(),
            None,
            256,
            token_code_hash.clone(),
            token_address.clone(),
        )?);
        output_msgs.push(snip20::set_viewing_key_msg(
            "SNIP20-ICS20".into(),
            None,
            256,
            token_code_hash.clone(),
            token_address.clone(),
        )?);
    }

    return Ok(output_msgs);
}

#[entry_point]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {}
}
