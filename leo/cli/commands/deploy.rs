// Copyright (C) 2019-2023 Aleo Systems Inc.
// This file is part of the Leo library.

// The Leo library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The Leo library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the Leo library. If not, see <https://www.gnu.org/licenses/>.

use super::*;
use aleo_std::StorageMode;
use leo_retriever::NetworkName;
use snarkvm::{
    circuit::{Aleo, AleoTestnetV0, AleoV0},
    cli::helpers::dotenv_private_key,
    ledger::query::Query as SnarkVMQuery,
    package::Package as SnarkVMPackage,
    prelude::{
        deployment_cost,
        store::{helpers::memory::ConsensusMemory, ConsensusStore},
        MainnetV0,
        PrivateKey,
        ProgramOwner,
        TestnetV0,
        VM,
    },
};
use std::{path::PathBuf, str::FromStr};
use text_tables;

/// Deploys an Aleo program.
#[derive(Parser, Debug)]
pub struct Deploy {
    #[clap(flatten)]
    pub(crate) fee_options: FeeOptions,
    #[clap(long, help = "Disables building of the project before deployment.", default_value = "false")]
    pub(crate) no_build: bool,
    #[clap(long, help = "Enables recursive deployment of dependencies.", default_value = "false")]
    pub(crate) recursive: bool,
    #[clap(
        long,
        help = "Time in seconds to wait between consecutive deployments. This is to help prevent a program from trying to be included in an earlier block than its dependency program.",
        default_value = "12"
    )]
    pub(crate) wait: u64,
    #[clap(flatten)]
    pub(crate) options: BuildOptions,
}

impl Command for Deploy {
    type Input = ();
    type Output = ();

    fn log_span(&self) -> Span {
        tracing::span!(tracing::Level::INFO, "Leo")
    }

    fn prelude(&self, context: Context) -> Result<Self::Input> {
        if !self.no_build {
            (Build { options: self.options.clone() }).execute(context)?;
        }
        Ok(())
    }

    fn apply(self, context: Context, _: Self::Input) -> Result<Self::Output> {
        // Parse the network.
        let network = NetworkName::try_from(self.options.network.as_str())?;
        match network {
            NetworkName::MainnetV0 => handle_deploy::<AleoV0, MainnetV0>(&self, context),
            NetworkName::TestnetV0 => handle_deploy::<AleoTestnetV0, TestnetV0>(&self, context),
        }
    }
}

// A helper function to handle deployment logic.
fn handle_deploy<A: Aleo<Network = N, BaseField = N::Field>, N: Network>(
    command: &Deploy,
    context: Context,
) -> Result<<Deploy as Command>::Output> {
    // Get the program name.
    let project_name = context.open_manifest::<N>()?.program_id().to_string();

    // Get the private key.
    let private_key = match &command.fee_options.private_key {
        Some(key) => PrivateKey::from_str(key)?,
        None => PrivateKey::from_str(
            &dotenv_private_key().map_err(CliError::failed_to_read_environment_private_key)?.to_string(),
        )?,
    };

    // Specify the query
    let query = SnarkVMQuery::from(&command.options.endpoint);

    let mut all_paths: Vec<(String, PathBuf)> = Vec::new();

    // Extract post-ordered list of local dependencies' paths from `leo.lock`.
    if command.recursive {
        // Cannot combine with private fee.
        if command.fee_options.record.is_some() {
            return Err(CliError::recursive_deploy_with_record().into());
        }
        all_paths = context.local_dependency_paths()?;
    }

    // Add the parent program to be deployed last.
    all_paths.push((project_name, context.dir()?.join("build")));

    for (index, (name, path)) in all_paths.iter().enumerate() {
        // Fetch the package from the directory.
        let package = SnarkVMPackage::<N>::open(path)?;

        println!("📦 Creating deployment transaction for '{}'...\n", &name.bold());

        // Generate the deployment
        let deployment = package.deploy::<A>(None)?;
        let deployment_id = deployment.to_deployment_id()?;

        let store = ConsensusStore::<N, ConsensusMemory<N>>::open(StorageMode::Production)?;

        // Initialize the VM.
        let vm = VM::from(store)?;

        // Compute the minimum deployment cost.
        let (mut total_cost, (storage_cost, synthesis_cost, namespace_cost)) = deployment_cost(&deployment)?;

        // Display the deployment cost breakdown using `credit` denomination.
        total_cost += command.fee_options.priority_fee;
        deploy_cost_breakdown(
            name,
            total_cost as f64 / 1_000_000.0,
            storage_cost as f64 / 1_000_000.0,
            synthesis_cost as f64 / 1_000_000.0,
            namespace_cost as f64 / 1_000_000.0,
            command.fee_options.priority_fee as f64 / 1_000_000.0,
        );

        // Initialize an RNG.
        let rng = &mut rand::thread_rng();

        // Prepare the fees.
        let fee = match &command.fee_options.record {
            Some(record) => {
                let fee_record = parse_record(&private_key, record)?;
                let fee_authorization = vm.authorize_fee_private(
                    &private_key,
                    fee_record,
                    total_cost,
                    command.fee_options.priority_fee,
                    deployment_id,
                    rng,
                )?;
                vm.execute_fee_authorization(fee_authorization, Some(query.clone()), rng)?
            }
            None => {
                // Make sure the user has enough public balance to pay for the deployment.
                check_balance(
                    &private_key,
                    &command.options.endpoint,
                    &command.options.network,
                    context.clone(),
                    total_cost,
                )?;
                let fee_authorization = vm.authorize_fee_public(
                    &private_key,
                    total_cost,
                    command.fee_options.priority_fee,
                    deployment_id,
                    rng,
                )?;
                vm.execute_fee_authorization(fee_authorization, Some(query.clone()), rng)?
            }
        };
        // Construct the owner.
        let owner = ProgramOwner::new(&private_key, deployment_id, rng)?;

        // Generate the deployment transaction.
        let transaction = Transaction::from_deployment(owner, deployment, fee)?;

        // Determine if the transaction should be broadcast, stored, or displayed to the user.
        if !command.fee_options.dry_run {
            println!("✅ Created deployment transaction for '{}'", name.bold());
            handle_broadcast(
                &format!("{}/{}/transaction/broadcast", command.options.endpoint, command.options.network),
                transaction,
                name,
            )?;
            // Wait between successive deployments to prevent out of order deployments.
            if index < all_paths.len() - 1 {
                std::thread::sleep(std::time::Duration::from_secs(command.wait));
            }
        } else {
            println!("✅ Successful dry run deployment for '{}'", name.bold());
        }
    }

    Ok(())
}

// A helper function to display a cost breakdown of the deployment.
fn deploy_cost_breakdown(
    name: &String,
    total_cost: f64,
    storage_cost: f64,
    synthesis_cost: f64,
    namespace_cost: f64,
    priority_fee: f64,
) {
    println!("Base deployment cost for '{}' is {} credits.", name.bold(), total_cost);
    // Display the cost breakdown in a table.
    let data = [
        [name, "Cost (credits)"],
        ["Transaction Storage", &format!("{:.6}", storage_cost)],
        ["Program Synthesis", &format!("{:.6}", synthesis_cost)],
        ["Namespace", &format!("{:.6}", namespace_cost)],
        ["Priority Fee", &format!("{:.6}", priority_fee)],
        ["Total", &format!("{:.6}", total_cost)],
    ];
    let mut out = Vec::new();
    text_tables::render(&mut out, data).unwrap();
    println!("{}", ::std::str::from_utf8(&out).unwrap());
}
