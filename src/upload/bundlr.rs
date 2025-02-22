pub use anchor_client::solana_sdk::native_token::LAMPORTS_PER_SOL;
use async_trait::async_trait;
use bundlr_sdk::{tags::Tag, Bundlr, SolanaSigner};
use clap::crate_version;
use console::style;
use futures::future::select_all;
use std::{
    cmp,
    collections::HashSet,
    ffi::OsStr,
    fs,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::time::{sleep, Duration};

use crate::candy_machine::ID as CANDY_MACHINE_ID;
use crate::{common::*, config::*, constants::PARALLEL_LIMIT, upload::*, utils::*};

/// The number os retries to fetch the Bundlr balance (MAX_RETRY * DELAY_UNTIL_RETRY ms limit)
const MAX_RETRY: u64 = 120;

/// Time (ms) to wait until next try
const DELAY_UNTIL_RETRY: u64 = 1000;

/// Size of Bundlr transaction header
const HEADER_SIZE: u64 = 2000;

/// Minimum file size for cost calculation
const MINIMUM_SIZE: u64 = 10000;

/// Size of the mock image URI for cost calculation
const MOCK_URI_SIZE: usize = 100;

struct TxInfo {
    asset_id: String,
    file_path: String,
    image_link: String,
    animation_link: Option<String>,
    data_type: DataType,
    tag: Vec<Tag>,
}

pub struct BundlrHandler {
    client: Arc<Bundlr<SolanaSigner>>,
    pubkey: Pubkey,
    node: String,
}

impl BundlrHandler {
    /// Initialize a new BundlrHandler.
    pub async fn initialize(
        config_data: &ConfigData,
        sugar_config: &SugarConfig,
    ) -> Result<BundlrHandler> {
        let client = setup_client(sugar_config)?;
        let program = client.program(CANDY_MACHINE_ID);
        let solana_cluster: Cluster = get_cluster(program.rpc())?;

        let bundlr_node = match config_data.upload_method {
            UploadMethod::Bundlr => match solana_cluster {
                Cluster::Devnet => BUNDLR_DEVNET,
                Cluster::Mainnet => BUNDLR_MAINNET,
            },
            _ => {
                return Err(anyhow!(format!(
                    "Upload method '{}' currently unsupported!",
                    &config_data.upload_method.to_string()
                )))
            }
        };

        let http_client = reqwest::Client::new();
        let bundlr_address =
            BundlrHandler::get_bundlr_solana_address(&http_client, bundlr_node).await?;

        let bundlr_pubkey = Pubkey::from_str(&bundlr_address)?;
        // get keypair as base58 string for Bundlr
        let keypair = bs58::encode(sugar_config.keypair.to_bytes()).into_string();
        let signer = SolanaSigner::from_base58(&keypair);

        let bundlr_client = Bundlr::new(
            bundlr_node.to_string(),
            "solana".to_string(),
            "sol".to_string(),
            signer,
        );

        Ok(BundlrHandler {
            client: Arc::new(bundlr_client),
            pubkey: bundlr_pubkey,
            node: bundlr_node.to_string(),
        })
    }

    /// Return the solana address for Bundlr.
    pub async fn get_bundlr_solana_address(http_client: &HttpClient, node: &str) -> Result<String> {
        let url = format!("{}/info", node);
        let data = http_client.get(&url).send().await?.json::<Value>().await?;
        let addresses = data
            .get("addresses")
            .expect("Failed to get bundlr addresses.");

        let solana_address = addresses
            .get("solana")
            .expect("Failed to get Solana address from bundlr.")
            .as_str()
            .expect("Solana bundlr address is not of type string.")
            .to_string();
        Ok(solana_address)
    }

    /// Add fund to the Bundlr address.
    pub async fn fund_bundlr_address(
        program: &Program,
        http_client: &HttpClient,
        bundlr_address: &Pubkey,
        node: &str,
        payer: &Keypair,
        amount: u64,
    ) -> Result<Response> {
        let ix = system_instruction::transfer(&payer.pubkey(), bundlr_address, amount);
        let recent_blockhash = program.rpc().get_latest_blockhash()?;
        let payer_pubkey = payer.pubkey();

        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&payer_pubkey),
            &[payer],
            recent_blockhash,
        );

        println!("Funding address:");
        println!("  -> pubkey: {}", payer_pubkey);
        println!(
            "  -> lamports: {} (◎ {})",
            amount,
            amount as f64 / LAMPORTS_PER_SOL as f64
        );

        let sig = program
            .rpc()
            .send_and_confirm_transaction_with_spinner_and_commitment(
                &tx,
                CommitmentConfig::confirmed(),
            )?;

        println!("{} {sig}", style("Signature:").bold());

        let mut map = HashMap::new();
        map.insert("tx_id", sig.to_string());
        let url = format!("{}/account/balance/solana", node);
        let response = http_client.post(&url).json(&map).send().await?;

        Ok(response)
    }

    /// Return the Bundlr balance.
    pub async fn get_bundlr_balance(
        http_client: &HttpClient,
        address: &str,
        node: &str,
    ) -> Result<u64> {
        debug!("Getting balance for address: {address}");
        let url = format!("{}/account/balance/solana/?address={}", node, address);
        let response = http_client.get(&url).send().await?.json::<Value>().await?;
        let value = response
            .get("balance")
            .expect("Failed to get balance from bundlr.");

        Ok(value
            .as_str()
            .unwrap()
            .parse::<u64>()
            .expect("Failed to parse bundlr balance."))
    }

    /// Return the Bundlr fee for upload based on the data size.
    pub async fn get_bundlr_fee(
        http_client: &HttpClient,
        node: &str,
        data_size: u64,
    ) -> Result<u64> {
        let required_amount = http_client
            .get(format!("{node}/price/solana/{data_size}"))
            .send()
            .await?
            .text()
            .await?
            .parse::<u64>()?;
        Ok(required_amount)
    }

    /// Send a transaction to Bundlr and wait for a response.
    async fn send_bundlr_tx(
        bundlr_client: Arc<Bundlr<SolanaSigner>>,
        tx_info: TxInfo,
    ) -> Result<(String, String)> {
        let data = match tx_info.data_type {
            DataType::Image => fs::read(&tx_info.file_path)?,
            DataType::Metadata => {
                // replaces the image link without modifying the original file to avoid
                // changing the hash of the metadata file
                get_updated_metadata(
                    &tx_info.file_path,
                    &tx_info.image_link,
                    tx_info.animation_link,
                )?
                .into_bytes()
            }
            DataType::Animation => fs::read(&tx_info.file_path)?,
        };

        let tx = bundlr_client.create_transaction_with_tags(data, tx_info.tag);
        let response = bundlr_client.send_transaction(tx).await?;
        let id = response
            .get("id")
            .expect("Failed to convert transaction id to string.")
            .as_str()
            .expect("Failed to get an id from bundlr transaction.");

        Ok((tx_info.asset_id, id.to_string()))
    }
}

#[async_trait]
impl UploadHandler for BundlrHandler {
    /// Funds Bundlr account for the upload.
    async fn prepare(
        &self,
        sugar_config: &SugarConfig,
        assets: &HashMap<usize, AssetPair>,
        image_indices: &[usize],
        metadata_indices: &[usize],
        animation_indices: &[usize],
    ) -> Result<()> {
        // calculates the size of the files to upload
        let mut total_size = 0;

        for index in image_indices {
            let item = assets.get(index).unwrap();
            let path = Path::new(&item.image);
            total_size += HEADER_SIZE + cmp::max(MINIMUM_SIZE, std::fs::metadata(path)?.len());
        }

        let mock_uri = "x".repeat(MOCK_URI_SIZE);

        if !animation_indices.is_empty() {
            for index in animation_indices {
                let item = assets.get(index).unwrap();
                let path = Path::new(item.animation.as_ref().unwrap());
                total_size += HEADER_SIZE + cmp::max(MINIMUM_SIZE, std::fs::metadata(path)?.len());
            }
        }

        for index in metadata_indices {
            let item = assets.get(index).unwrap();

            let mock_animation_uri = if item.animation.is_some() {
                Some("x".repeat(MOCK_URI_SIZE))
            } else {
                None
            };

            let updated_metadata =
                match get_updated_metadata(&item.metadata, &mock_uri, mock_animation_uri.clone()) {
                    Ok(metadata) => metadata.into_bytes().len() as u64,
                    Err(err) => return Err(err),
                };

            total_size += HEADER_SIZE + cmp::max(MINIMUM_SIZE, updated_metadata);
        }

        info!("Total upload size: {}", total_size);

        let http_client = reqwest::Client::new();

        let lamports_fee = BundlrHandler::get_bundlr_fee(&http_client, &self.node, total_size)
            .await?
            * (1.1 as u64);

        let address = sugar_config.keypair.pubkey().to_string();
        let mut balance =
            BundlrHandler::get_bundlr_balance(&http_client, &address, &self.node).await?;

        info!(
            "Bundlr balance {} lamports, require {} lamports",
            balance, lamports_fee
        );

        // funds the bundlr wallet for image upload

        let client = setup_client(sugar_config)?;
        let program = client.program(CANDY_MACHINE_ID);

        if lamports_fee > balance {
            BundlrHandler::fund_bundlr_address(
                &program,
                &http_client,
                &self.pubkey,
                &self.node,
                &sugar_config.keypair,
                lamports_fee - balance,
            )
            .await?;

            let pb = ProgressBar::new(MAX_RETRY);
            pb.set_style(ProgressStyle::default_bar().template("{spinner} {msg} {wide_bar}"));
            pb.enable_steady_tick(60);
            pb.set_message("Verifying balance:");

            // waits until the balance can be verified, otherwise the upload
            // will fail
            for _i in 0..MAX_RETRY {
                let res =
                    BundlrHandler::get_bundlr_balance(&http_client, &address, &self.node).await;

                if let Ok(value) = res {
                    balance = value;
                }

                if balance >= lamports_fee {
                    break;
                }

                sleep(Duration::from_millis(DELAY_UNTIL_RETRY)).await;
                pb.inc(1);
            }

            pb.finish_and_clear();

            if balance < lamports_fee {
                let error = UploadError::NoBundlrBalance(address).into();
                error!("{error}");
                return Err(error);
            }
        }

        Ok(())
    }

    /// Upload the data to Bundlr.
    async fn upload_data(
        &self,
        _sugar_config: &SugarConfig,
        assets: &HashMap<usize, AssetPair>,
        cache: &mut Cache,
        indices: &[usize],
        data_type: DataType,
        interrupted: Arc<AtomicBool>,
    ) -> Result<Vec<UploadError>> {
        let mut extension = HashSet::with_capacity(1);
        let mut paths = Vec::new();

        for index in indices {
            let item = match assets.get(index) {
                Some(asset_index) => asset_index,
                None => return Err(anyhow::anyhow!("Failed to get asset at index {}", index)),
            };
            // chooses the file path based on the data type
            let file_path = match data_type {
                DataType::Image => item.image.clone(),
                DataType::Metadata => item.metadata.clone(),
                DataType::Animation => item.animation.clone().unwrap(),
            };

            let path = Path::new(&file_path);
            let ext = path
                .extension()
                .and_then(OsStr::to_str)
                .expect("Failed to convert extension from unicode");
            extension.insert(String::from(ext));

            paths.push(file_path);
        }

        // validates that all files have the same extension
        let extension = if extension.len() == 1 {
            extension.iter().next().unwrap()
        } else {
            return Err(anyhow!("Invalid file extension: {:?}", extension));
        };

        let sugar_tag = Tag::new("App-Name".into(), format!("Sugar {}", crate_version!()));

        let image_tag = match data_type {
            DataType::Image => Tag::new("Content-Type".into(), format!("image/{extension}")),
            DataType::Metadata => Tag::new("Content-Type".into(), "application/json".to_string()),
            DataType::Animation => Tag::new("Content-Type".into(), format!("video/{extension}")),
        };

        // upload data to bundlr

        println!("\nSending data: (Ctrl+C to abort)");

        let pb = progress_bar_with_style(paths.len() as u64);
        let mut transactions = Vec::new();

        for file_path in paths {
            // path to the image/metadata file
            let path = Path::new(&file_path);

            // id of the asset (to be used to update the cache link)
            let asset_id = String::from(
                path.file_stem()
                    .and_then(OsStr::to_str)
                    .expect("Failed to convert path to unicode."),
            );

            let cache_item = match cache.items.0.get(&asset_id) {
                Some(item) => item,
                None => return Err(anyhow!("Failed to get config item at index {}", asset_id)),
            };

            // todo make sure if failure it should be empty string, this makes it able to be reuploaded if animation present

            transactions.push(TxInfo {
                asset_id: asset_id.to_string(),
                file_path: String::from(path.to_str().expect("Failed to parse path from unicode.")),
                image_link: cache_item.image_link.clone(),
                data_type: data_type.clone(),
                tag: vec![sugar_tag.clone(), image_tag.clone()],
                animation_link: cache_item.animation_link.clone(),
            });
        }

        let mut handles = Vec::new();

        for tx in transactions.drain(0..cmp::min(transactions.len(), PARALLEL_LIMIT)) {
            let bundlr_client = self.client.clone();
            handles.push(tokio::spawn(async move {
                BundlrHandler::send_bundlr_tx(bundlr_client, tx).await
            }));
        }

        let mut errors = Vec::new();

        while !interrupted.load(Ordering::SeqCst) && !handles.is_empty() {
            match select_all(handles).await {
                (Ok(res), _index, remaining) => {
                    // independently if the upload was successful or not
                    // we continue to try the remaining ones
                    handles = remaining;

                    if res.is_ok() {
                        let val = res?;
                        let link = format!("https://arweave.net/{}", val.clone().1);
                        // cache item to update
                        let item = cache.items.0.get_mut(&val.0).unwrap();

                        match data_type {
                            DataType::Image => item.image_link = link,
                            DataType::Metadata => item.metadata_link = link,
                            DataType::Animation => item.animation_link = Some(link),
                        }
                        // updates the progress bar
                        pb.inc(1);
                    } else {
                        // user will need to retry the upload
                        errors.push(UploadError::SendDataFailed(format!(
                            "Bundlr upload error: {:?}",
                            res.err().unwrap()
                        )));
                    }
                }
                (Err(err), _index, remaining) => {
                    errors.push(UploadError::SendDataFailed(format!(
                        "Bundlr upload error: {:?}",
                        err
                    )));
                    // ignoring all errors
                    handles = remaining;
                }
            }

            if !transactions.is_empty() {
                // if we are half way through, let spawn more transactions
                if (PARALLEL_LIMIT - handles.len()) > (PARALLEL_LIMIT / 2) {
                    // syncs cache (checkpoint)
                    cache.sync_file()?;

                    for tx in
                        transactions.drain(0..cmp::min(transactions.len(), PARALLEL_LIMIT / 2))
                    {
                        let bundlr_client = self.client.clone();
                        handles.push(tokio::spawn(async move {
                            BundlrHandler::send_bundlr_tx(bundlr_client, tx).await
                        }));
                    }
                }
            }
        }

        if !errors.is_empty() {
            pb.abandon_with_message(format!("{}", style("Upload failed ").red().bold()));
        } else if !transactions.is_empty() {
            pb.abandon_with_message(format!("{}", style("Upload aborted ").red().bold()));
            return Err(
                UploadError::SendDataFailed("Not all files were uploaded.".to_string()).into(),
            );
        } else {
            pb.finish_with_message(format!("{}", style("Upload successful ").green().bold()));
        }

        // makes sure the cache file is updated
        cache.sync_file()?;

        Ok(errors)
    }
}
