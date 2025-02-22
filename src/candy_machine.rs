use anchor_client::solana_sdk::pubkey::Pubkey;
use anchor_client::Client;
use anchor_lang::AccountDeserialize;
use anyhow::{anyhow, Result};

use mpl_candy_machine::{CandyMachine, CandyMachineData, WhitelistMintMode, WhitelistMintSettings};

use crate::config::data::SugarConfig;
use crate::config::{price_as_lamports, ConfigData};
use crate::setup::setup_client;

use crate::utils::check_spl_token;

pub use mpl_candy_machine::ID;
use spl_token::id as token_program_id;
// To test a custom candy machine program, comment the line above and use the
// following lines to declare the id to use:
//use solana_program::declare_id;
//declare_id!("<CANDY MACHINE ID>");

#[derive(Debug)]
pub struct ConfigStatus {
    pub index: u32,
    pub on_chain: bool,
}

pub fn parse_config_price(client: &Client, config: &ConfigData) -> Result<u64> {
    let parsed_price = if let Some(spl_token) = config.spl_token {
        let token_program = client.program(token_program_id());
        let token_mint = check_spl_token(&token_program, &spl_token.to_string())?;

        match (config.price as u64).checked_mul(10u64.pow(token_mint.decimals.into())) {
            Some(price) => price,
            None => return Err(anyhow!("Price math overflow")),
        }
    } else {
        price_as_lamports(config.price)
    };

    Ok(parsed_price)
}

pub fn get_candy_machine_state(
    sugar_config: &SugarConfig,
    candy_machine_id: &Pubkey,
) -> Result<CandyMachine> {
    let client = setup_client(sugar_config)?;
    let program = client.program(ID);

    let data = program.rpc().get_account_data(candy_machine_id)?;
    let candy_machine: CandyMachine = CandyMachine::try_deserialize(&mut data.as_slice())?;

    Ok(candy_machine)
}

pub fn get_candy_machine_data(
    sugar_config: &SugarConfig,
    candy_machine_id: &Pubkey,
) -> Result<CandyMachineData> {
    let candy_machine = get_candy_machine_state(sugar_config, candy_machine_id)?;
    Ok(candy_machine.data)
}

pub fn uuid_from_pubkey(pubkey: &Pubkey) -> String {
    pubkey.to_string()[0..6].to_string()
}

pub fn print_candy_machine_state(state: CandyMachine) {
    println!("Authority {:?}", state.authority);
    println!("Wallet {:?}", state.wallet);
    println!("Token mint: {:?}", state.token_mint);
    println!("Items redeemed: {:?}", state.items_redeemed);
    print_candy_machine_data(&state.data);
}

pub fn print_candy_machine_data(data: &CandyMachineData) {
    println!("Uuid: {:?}", data.uuid);
    println!("Price: {:?}", data.price);
    println!("Symbol: {:?}", data.symbol);
    println!(
        "Seller fee basis points: {:?}",
        data.seller_fee_basis_points
    );
    println!("Max supply: {:?}", data.max_supply);
    println!("Is mutable: {:?}", data.is_mutable);
    println!("Retain Authority: {:?}", data.retain_authority);
    println!("Go live date: {:?}", data.go_live_date);
    println!("Items available: {:?}", data.items_available);

    print_whitelist_mint_settings(&data.whitelist_mint_settings);
}

fn print_whitelist_mint_settings(settings: &Option<WhitelistMintSettings>) {
    if let Some(settings) = settings {
        match settings.mode {
            WhitelistMintMode::BurnEveryTime => println!("Mode: Burn every time"),
            WhitelistMintMode::NeverBurn => println!("Mode: Never burn"),
        }
        println!("Mint: {:?}", settings.mint);
        println!("Presale: {:?}", settings.presale);
        println!("Discount price: {:?}", settings.discount_price);
    } else {
        println!("No whitelist mint settings");
    }
}
