use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use cosmwasm_std::{
    attr, entry_point, from_binary, to_binary, Addr, Binary, CosmosMsg, DepsMut, Env,
    Ibc3ChannelOpenResponse, IbcBasicResponse, IbcChannel, IbcChannelCloseMsg,
    IbcChannelConnectMsg, IbcChannelOpenMsg, IbcEndpoint, IbcOrder, IbcPacket, IbcPacketAckMsg,
    IbcPacketReceiveMsg, IbcPacketTimeoutMsg, IbcReceiveResponse, Reply, Response, SubMsg,
    SubMsgResult, Uint128, WasmMsg,
};

use crate::amount::Snip20Coin;
use crate::error::{ContractError, Never};

use crate::state::{
    reduce_channel_balance, undo_reduce_channel_balance, ChannelInfo, ReplyArgs, CHANNEL_INFO,
    CODE_HASH, REPLY_ARGS,
};

pub const ICS20_VERSION: &str = "ics20-1";
pub const ICS20_ORDERING: IbcOrder = IbcOrder::Unordered;

/// The format for sending an ics20 packet.
/// Proto defined here: https://github.com/cosmos/cosmos-sdk/blob/v0.42.0/proto/ibc/applications/transfer/v1/transfer.proto#L11-L20
/// This is compatible with the JSON serialization
#[derive(Serialize, Deserialize, Clone, Eq, PartialEq, JsonSchema, Debug, Default)]
pub struct Ics20Packet {
    /// amount of tokens to transfer is encoded as a string, but limited to u64 max
    pub amount: Uint128,
    /// the token denomination to be transferred
    pub denom: String,
    /// the recipient address on the destination chain
    pub receiver: String,
    /// the sender address
    pub sender: String,
}

impl Ics20Packet {
    pub fn new<T: Into<String>>(amount: Uint128, denom: T, sender: &str, receiver: &str) -> Self {
        Ics20Packet {
            denom: denom.into(),
            amount,
            sender: sender.to_string(),
            receiver: receiver.to_string(),
        }
    }

    pub fn validate(&self) -> Result<(), ContractError> {
        if self.amount.u128() > (u64::MAX as u128) {
            Err(ContractError::AmountOverflow {})
        } else if self.amount.u128() == 0 {
            Err(ContractError::NoFunds {})
        } else {
            Ok(())
        }
    }
}

/// This is a generic ICS acknowledgement format.
/// Proto defined here: https://github.com/cosmos/cosmos-sdk/blob/v0.42.0/proto/ibc/core/channel/v1/channel.proto#L141-L147
/// This is compatible with the JSON serialization
#[derive(Serialize, Deserialize, Clone, Eq, PartialEq, JsonSchema, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Ics20Ack {
    Result(Binary),
    Error(String),
}

// create a serialized success message
fn ack_success() -> Binary {
    let res = Ics20Ack::Result(b"1".into());
    to_binary(&res).unwrap()
}

// create a serialized error message
fn ack_fail(err: String) -> Binary {
    let res = Ics20Ack::Error(err);
    to_binary(&res).unwrap()
}

const RECEIVE_ID: u64 = 1337;
const ACK_FAILURE_ID: u64 = 0xfa17;

#[entry_point]
pub fn reply(deps: DepsMut, _env: Env, reply: Reply) -> Result<Response, ContractError> {
    match reply.id {
        RECEIVE_ID => match reply.result {
            SubMsgResult::Ok(_) => Ok(Response::new()),
            SubMsgResult::Err(err) => {
                // Important design note:  with ibcv2 and wasmd 0.22 we can implement this all much easier.
                // No reply needed... the receive function and submessage should return error on failure and all
                // state gets reverted with a proper app-level message auto-generated

                // Since we need compatibility with Juno (Jan 2022), we need to ensure that optimisitic
                // state updates in ibc_packet_receive get reverted in the (unlikely) chance of an
                // error while sending the token

                // However, this requires passing some state between the ibc_packet_receive function and
                // the reply handler. We do this with a singleton, with is "okay" for IBC as there is no
                // reentrancy on these functions (cannot be called by another contract). This pattern
                // should not be used for ExecuteMsg handlers
                let reply_args = REPLY_ARGS.load(deps.storage)?;
                undo_reduce_channel_balance(
                    deps.storage,
                    &reply_args.channel,
                    &reply_args.denom,
                    reply_args.amount,
                )?;

                Ok(Response::new()
                    .add_attribute("ack_error", &err)
                    .set_data(ack_fail(err)))
            }
        },
        ACK_FAILURE_ID => match reply.result {
            SubMsgResult::Ok(_) => Ok(Response::new()),
            SubMsgResult::Err(err) => Ok(Response::new().set_data(ack_fail(err))),
        },
        _ => Err(ContractError::UnknownReplyId { id: reply.id }),
    }
}

#[entry_point]
/// enforces ordering and versioning constraints
pub fn ibc_channel_open(
    _deps: DepsMut,
    _env: Env,
    msg: IbcChannelOpenMsg,
) -> Result<Option<Ibc3ChannelOpenResponse>, ContractError> {
    enforce_order_and_version(msg.channel(), msg.counterparty_version())?;
    Ok(None)
}

#[entry_point]
/// record the channel in CHANNEL_INFO
pub fn ibc_channel_connect(
    deps: DepsMut,
    _env: Env,
    msg: IbcChannelConnectMsg,
) -> Result<IbcBasicResponse, ContractError> {
    // we need to check the counter party version in try and ack (sometimes here)
    enforce_order_and_version(msg.channel(), msg.counterparty_version())?;

    let channel: IbcChannel = msg.into();
    let info = ChannelInfo {
        id: channel.endpoint.channel_id,
        counterparty_endpoint: channel.counterparty_endpoint,
        connection_id: channel.connection_id,
    };
    CHANNEL_INFO.save(deps.storage, &info.id, &info)?;

    Ok(IbcBasicResponse::default())
}

fn enforce_order_and_version(
    channel: &IbcChannel,
    counterparty_version: Option<&str>,
) -> Result<(), ContractError> {
    if channel.version != ICS20_VERSION {
        return Err(ContractError::InvalidIbcVersion {
            version: channel.version.clone(),
        });
    }
    if let Some(version) = counterparty_version {
        if version != ICS20_VERSION {
            return Err(ContractError::InvalidIbcVersion {
                version: version.to_string(),
            });
        }
    }
    if channel.order != ICS20_ORDERING {
        return Err(ContractError::OnlyOrderedChannel {});
    }
    Ok(())
}

#[entry_point]
pub fn ibc_channel_close(
    _deps: DepsMut,
    _env: Env,
    _channel: IbcChannelCloseMsg,
) -> Result<IbcBasicResponse, ContractError> {
    // TODO: what to do here?
    // we will have locked funds that need to be returned somehow
    unimplemented!();
}

#[entry_point]
/// Check to see if we have any balance here
/// We should not return an error if possible, but rather an acknowledgement of failure
pub fn ibc_packet_receive(
    deps: DepsMut,
    _env: Env,
    msg: IbcPacketReceiveMsg,
) -> Result<IbcReceiveResponse, Never> {
    let packet = msg.packet;

    do_ibc_packet_receive(deps, &packet).or_else(|err| {
        Ok(IbcReceiveResponse::new()
            .set_ack(ack_fail(err.to_string()))
            .add_attributes(vec![
                attr("action", "receive"),
                attr("success", "false"),
                attr("error", err.to_string()),
            ]))
    })
}

// Returns local denom if the denom is an encoded voucher from the expected endpoint
// Otherwise, error
fn parse_voucher_denom<'a>(
    voucher_denom: &'a str,
    remote_endpoint: &IbcEndpoint,
) -> Result<&'a str, ContractError> {
    let split_denom: Vec<&str> = voucher_denom.splitn(3, '/').collect();
    if split_denom.len() != 3 {
        return Err(ContractError::NoForeignTokens {});
    }
    // a few more sanity checks
    if split_denom[0] != remote_endpoint.port_id {
        return Err(ContractError::FromOtherPort {
            port: split_denom[0].into(),
        });
    }
    if split_denom[1] != remote_endpoint.channel_id {
        return Err(ContractError::FromOtherChannel {
            channel: split_denom[1].into(),
        });
    }

    if !split_denom[2].starts_with("cw20:secret1") {
        return Err(ContractError::OnlySecretTokens {});
    }

    let token_address = split_denom[2].get(5..).unwrap();

    Ok(token_address)
}

// this does the work of ibc_packet_receive, we wrap it to turn errors into acknowledgements
fn do_ibc_packet_receive(
    deps: DepsMut,
    packet: &IbcPacket,
) -> Result<IbcReceiveResponse, ContractError> {
    let msg: Ics20Packet = from_binary(&packet.data)?;
    let channel = packet.dest.channel_id.clone();

    // If the token originated on the remote chain, it looks like "ucosm".
    // If it originated on our chain, it looks like "port/channel/cw20:...".
    let token_address = parse_voucher_denom(&msg.denom, &packet.src)?;
    let code_hash = CODE_HASH.load(deps.storage, Addr::unchecked(token_address))?;

    // make sure we have enough balance for this
    reduce_channel_balance(deps.storage, &channel, token_address, msg.amount)?;

    // we need to save the data to update the balances in reply
    let reply_args = ReplyArgs {
        channel,
        denom: token_address.to_string(),
        amount: msg.amount,
    };
    REPLY_ARGS.save(deps.storage, &reply_args)?;

    deps.api.debug(&format!(
        "do_ibc_packet_receive() token={} code_hash={} receiver={} amount={}",
        token_address,
        code_hash,
        msg.receiver.clone(),
        msg.amount
    ));

    let transfer = transfer_amount(
        token_address.to_string(),
        code_hash,
        msg.receiver.clone(),
        msg.amount,
    );

    deps.api
        .debug(&format!("do_ibc_packet_receive() transfer={:?}", transfer));

    let submsg = SubMsg::reply_on_error(transfer, RECEIVE_ID);

    let res = IbcReceiveResponse::new()
        .set_ack(ack_success())
        .add_submessage(submsg)
        .add_attribute("action", "receive")
        .add_attribute("sender", msg.sender)
        .add_attribute("receiver", msg.receiver)
        .add_attribute("denom", token_address)
        .add_attribute("amount", msg.amount)
        .add_attribute("success", "true");

    Ok(res)
}

#[entry_point]
/// check if success or failure and update balance, or return funds
pub fn ibc_packet_ack(
    deps: DepsMut,
    _env: Env,
    msg: IbcPacketAckMsg,
) -> Result<IbcBasicResponse, ContractError> {
    // Design decision: should we trap error like in receive?
    // TODO: unsure... as it is now a failed ack handling would revert the tx and would be
    // retried again and again. is that good?
    let ics20msg: Ics20Ack = from_binary(&msg.acknowledgement.data)?;
    match ics20msg {
        Ics20Ack::Result(_) => on_packet_success(deps, msg.original_packet),
        Ics20Ack::Error(err) => on_packet_failure(deps, msg.original_packet, err),
    }
}

#[entry_point]
/// return fund to original sender (same as failure in ibc_packet_ack)
pub fn ibc_packet_timeout(
    deps: DepsMut,
    _env: Env,
    msg: IbcPacketTimeoutMsg,
) -> Result<IbcBasicResponse, ContractError> {
    // TODO: trap error like in receive? (same question as ack above)
    let packet = msg.packet;
    on_packet_failure(deps, packet, "timeout".to_string())
}

// update the balance stored on this (channel, denom) index
fn on_packet_success(_deps: DepsMut, packet: IbcPacket) -> Result<IbcBasicResponse, ContractError> {
    let msg: Ics20Packet = from_binary(&packet.data)?;

    // similar event messages like ibctransfer module
    let attributes = vec![
        attr("action", "acknowledge"),
        attr("sender", &msg.sender),
        attr("receiver", &msg.receiver),
        attr("denom", &msg.denom),
        attr("amount", msg.amount),
        attr("success", "true"),
    ];

    Ok(IbcBasicResponse::new().add_attributes(attributes))
}

// return the tokens to sender
fn on_packet_failure(
    deps: DepsMut,
    packet: IbcPacket,
    err: String,
) -> Result<IbcBasicResponse, ContractError> {
    let msg: Ics20Packet = from_binary(&packet.data)?;

    // undo the balance update on failure (as we pre-emptively added it on send)
    reduce_channel_balance(deps.storage, &packet.src.channel_id, &msg.denom, msg.amount)?;

    let to_send = Snip20Coin::from_parts(msg.denom.clone(), msg.amount);
    let code_hash = CODE_HASH.load(deps.storage, deps.api.addr_validate(&to_send.address)?)?;

    let sender = deps.api.addr_validate(&msg.sender)?;
    let send = transfer_amount(
        to_send.address,
        code_hash,
        sender.into_string(),
        to_send.amount,
    );
    let submsg = SubMsg::reply_on_error(send, ACK_FAILURE_ID);

    // similar event messages like ibctransfer module
    let res = IbcBasicResponse::new()
        .add_submessage(submsg)
        .add_attribute("action", "acknowledge")
        .add_attribute("sender", msg.sender)
        .add_attribute("receiver", msg.receiver)
        .add_attribute("denom", msg.denom)
        .add_attribute("amount", msg.amount.to_string())
        .add_attribute("success", "false")
        .add_attribute("error", err);

    Ok(res)
}

fn transfer_amount(
    contract_addr: String,
    code_hash: String,
    recipient: String,
    amount: Uint128,
) -> CosmosMsg {
    WasmMsg::Execute {
        contract_addr,
        code_hash,
        msg: Binary::from(
            format!(
                r#"{{"transfer":{{"recipient":"{}","amount":"{}"}}}}"#,
                recipient,
                amount.u128()
            )
            .as_bytes()
            .to_vec(),
        ),
        funds: vec![],
    }
    .into()
}
