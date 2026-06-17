/*
 * SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: Apache-2.0
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 * http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs;

use axum::Router;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum_template::TemplateEngine;
use base64::Engine as _;
use carbide_host_support::agent_config;
use carbide_uuid::machine::{MachineId, MachineInterfaceId};
use rpc::forge;
use rpc::forge::PxeDomain;

use crate::common::{AppState, Machine};

const DEFAULT_NUM_OF_VFS: u32 = 16;

/// Generates the content of the /etc/forge/config.toml file.
///
/// When `api_url_override` is provided (for external hosts on the
/// static-assignments segment), it's written into the `[forge-system]`
/// section so the DPU agent connects to the correct API endpoint
/// instead of defaulting to `carbide-api.forge`.
fn generate_forge_agent_config(
    machine_interface_id: MachineInterfaceId,
    api_url_override: Option<&str>,
) -> String {
    let config = agent_config::AgentConfigFromPxe {
        forge_system: api_url_override.map(|url| agent_config::ForgeSystemConfigFromPxe {
            api_server: url.to_string(),
        }),
        machine: agent_config::MachineConfigFromPxe {
            interface_id: machine_interface_id,
        },
    };

    toml::to_string(&config).unwrap_or_else(|e| format!("# serialization error: {e}"))
}

fn print_and_generate_generic_error(error: String) -> (String, HashMap<String, String>) {
    eprintln!("{error}");
    let mut template_data: HashMap<String, String> = HashMap::new();
    template_data.insert(
        "error".to_string(),
        "An error occurred while rendering the request".to_string(),
    );
    ("error".to_string(), template_data) // Send a generic error back
}

/// in the OK path returns either an Empty vec if no files are found, or a vec of tuples with (basename, url),
/// or an Error.
async fn get_cloud_init_urls(
    custom_cloud_init_includes_directory: &str,
    server_name: &str,
    web_root: &str,
) -> Result<Option<Vec<(String, String)>>, Box<dyn std::error::Error>> {
    let dir_path = Path::new(custom_cloud_init_includes_directory);

    let mut dir_entries = fs::read_dir(dir_path).await?;
    let mut entries: Vec<(String, PathBuf)> = Vec::new();

    while let Some(entry) = dir_entries.next_entry().await? {
        let path = entry.path();
        if path.is_file() {
            if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                entries.push((name.to_string(), path));
            }
        }
    }

    if entries.is_empty() {
        return Ok(None);
    }

    // Sort by filename (the .0 element)
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let base_url = format!(
        "{}/{}",
        server_name.trim_matches('/'),
        web_root.trim_matches('/')
    );

    Ok(Some(
        entries
            .into_iter()
            .map(|(name, _path)| {
                let url = format!("{}/{}", base_url, name);
                (name, url)
            })
            .collect(),
    ))
}
#[allow(clippy::too_many_arguments)]
async fn user_data_handler(
    machine_interface_id: MachineInterfaceId,
    machine_interface: forge::MachineInterface,
    domain: PxeDomain,
    hbn_reps: Option<String>,
    hbn_sfs: Option<String>,
    num_of_vfs: Option<u32>,
    vf_intercept_bridge_name: Option<String>,
    host_intercept_bridge_name: Option<String>,
    host_intercept_bridge_port: Option<String>,
    vf_intercept_bridge_port: Option<String>,
    vf_intercept_bridge_sf: Option<String>,
    api_url_override: Option<String>,
    pxe_url_override: Option<String>,
    state: State<AppState>,
) -> (String, HashMap<String, String>) {
    let config = state.runtime_config.clone();
    let forge_agent_config =
        generate_forge_agent_config(machine_interface_id, api_url_override.as_deref());

    let mut context: HashMap<String, String> = HashMap::new();
    context.insert("mac_address".to_string(), machine_interface.mac_address);

    if let Some(domain_oneof) = domain.domain {
        match domain_oneof {
            forge::pxe_domain::Domain::LegacyDomain(domain) => {
                context.insert("hostname".to_string(), domain.name);
            }
            forge::pxe_domain::Domain::NewDomain(domain) => {
                context.insert("hostname".to_string(), domain.name);
            }
        }
    }
    context.insert("interface_id".to_string(), machine_interface_id.to_string());
    // Use URL overrides for external clients (static-assignments segment),
    // falling back to global config.
    context.insert(
        "api_url".to_string(),
        api_url_override.unwrap_or(config.client_facing_api_url),
    );
    context.insert(
        "pxe_url".to_string(),
        pxe_url_override.unwrap_or(config.pxe_url.clone()),
    );
    context.insert(
        "forge_agent_config_b64".to_string(),
        base64::engine::general_purpose::STANDARD.encode(forge_agent_config),
    );

    let bmc_fw_update = state
        .engine
        .render("bmc_fw_update", HashMap::<String, String>::new())
        .unwrap_or("".to_string());
    context.insert("forge_bmc_fw_update".to_string(), bmc_fw_update);

    let seconds_since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(std::time::Duration::ZERO)
        .as_secs();

    context.insert(
        "seconds_since_epoch".to_string(),
        seconds_since_epoch.to_string(),
    );

    if let Some(hbn_reps) = hbn_reps {
        context.insert("forge_hbn_reps".to_string(), hbn_reps);
    }

    if let Some(hbn_sfs) = hbn_sfs {
        context.insert("forge_hbn_sfs".to_string(), hbn_sfs);
    }

    let num_of_vfs = num_of_vfs.unwrap_or(DEFAULT_NUM_OF_VFS);
    context.insert("num_of_vfs".to_string(), num_of_vfs.to_string());

    if let Some(vf_intercept_bridge_name) = vf_intercept_bridge_name {
        context.insert(
            "forge_vf_intercept_bridge_name".to_string(),
            vf_intercept_bridge_name,
        );
    }

    if let Some(host_intercept_bridge_name) = host_intercept_bridge_name {
        context.insert(
            "forge_host_intercept_bridge_name".to_string(),
            host_intercept_bridge_name,
        );
    }

    if let Some(host_intercept_bridge_port) = host_intercept_bridge_port {
        context.insert(
            "forge_host_intercept_hbn_port".to_string(),
            format!("patch-hbn-{host_intercept_bridge_port}"),
        );

        context.insert(
            "forge_host_intercept_bridge_port".to_string(),
            host_intercept_bridge_port,
        );
    }

    if let Some(vf_intercept_bridge_port) = vf_intercept_bridge_port {
        context.insert(
            "forge_vf_intercept_hbn_port".to_string(),
            format!("patch-hbn-{vf_intercept_bridge_port}"),
        );

        context.insert(
            "forge_vf_intercept_bridge_port".to_string(),
            vf_intercept_bridge_port,
        );
    }

    if let Some(vf_intercept_bridge_sf) = vf_intercept_bridge_sf {
        context.insert(
            "forge_vf_intercept_bridge_sf_representor".to_string(),
            format!("{vf_intercept_bridge_sf}_r"),
        );

        context.insert(
            "forge_vf_intercept_bridge_sf_hbn_bridge_representor".to_string(),
            format!("{vf_intercept_bridge_sf}_if_r"),
        );

        context.insert(
            "forge_vf_intercept_bridge_sf".to_string(),
            vf_intercept_bridge_sf,
        );
    }

    let custom_cloud_init_includes_directory = std::env::var("CUSTOM_CLOUD_INCLUDE_DIR")
        .unwrap_or("/forge-boot-artifacts/blobs/internal/cloud-init.d/dpu/".to_string());
    let custom_cloud_init_web_root = std::env::var("CUSTOM_CLOUD_INIT_WEB_ROOT")
        .unwrap_or("public/blobs/internal/cloud-init.d/dpu/".to_string());

    // Define the staging path on the DPU filesystem
    let staging_dir = "/opt/forge/custom-cloud-init.d/";

    let (download_block, execute_block) = get_cloud_init_urls(
        custom_cloud_init_includes_directory.as_str(),
        config.pxe_url.as_str(),
        custom_cloud_init_web_root.as_str(),
    )
        .await
        .map_err(|err| eprintln!("error reading custom cloud init files: {err:?}"))
        .unwrap_or_default()
        .map(|pairs| {
            let download_cmds = pairs.iter()
                .map(|(name, url)| {
                    format!("curl -L --retry 5 --retry-all-errors -v -o /mnt{}{} {}", staging_dir, name, url)
                })
                .collect::<Vec<_>>()
                .join("\n");

            let downloads = format!(
                "\n# Start External Snippet Injection\nmkdir -p /mnt{}\n{}\n# End External Snippet Injection\n",
                staging_dir, download_cmds
            );

            //TODO: think about putting the execute lines into an `include!` maybe? -- it would still have to be run through a `format!` macro, but maybe that's cleaner?
            let execute_lines = vec![
                "# --- Start Cloud-Init Snippet Execution ---".to_string(),
                "CLOUD_DIR=\"/var/lib/cloud\"".to_string(),
                "BACKUP_DIR=\"/var/lib/cloud.original\"".to_string(),
                "FORGE_CFG=\"/etc/cloud/cloud.cfg.d/99-forge-snippet.cfg\"".to_string(),
                "".to_string(),
                "# 1. Create a Time Capsule of the original state".to_string(),
                "if [ ! -d \"$BACKUP_DIR\" ]; then cp -rp \"$CLOUD_DIR\" \"$BACKUP_DIR\"; fi".to_string(),
                "".to_string(),
                format!("if [ -d \"{staging_dir}\" ]; then"),
                format!("    for snippet in {staging_dir}*; do"),
                "        [ -e \"$snippet\" ] || continue".to_string(),
                "        echo \"Processing arbitrary snippet: $snippet\"".to_string(),
                "".to_string(),
                "        # 2. Infiltrate global config and wipe active instance memory".to_string(),
                "        cp \"$snippet\" \"$FORGE_CFG\"".to_string(),
                "        rm -rf \"${CLOUD_DIR}/instance\" \"${CLOUD_DIR}/instances\" \"${CLOUD_DIR}/data\"".to_string(),
                "".to_string(),
                "        # 3. Chained execution: stop immediately if any stage fails".to_string(),
                "        (".to_string(),
                "            ip vrf exec mgmt cloud-init init --local && \\".to_string(),
                "            ip vrf exec mgmt cloud-init modules --mode config && \\".to_string(),
                "            ip vrf exec mgmt cloud-init modules --mode final".to_string(),
                "        ) || echo \"ERROR: Cloud-init execution failed for $snippet. Skipping to cleanup.\"".to_string(),
                "".to_string(),
                "        # 4. Clean up the injection point for the next loop iteration".to_string(),
                "        rm -f \"$FORGE_CFG\"".to_string(),
                "    done".to_string(),
                "fi".to_string(),
                "".to_string(),
                "# 5. Restore the 'Time Capsule' so the DPU state is preserved".to_string(),
                "echo \"Restoring original cloud-init state...\"".to_string(),
                "rm -rf \"$CLOUD_DIR\" && mv \"$BACKUP_DIR\" \"$CLOUD_DIR\"".to_string(),
                "# --- End Cloud-Init Snippet Execution ---".to_string(),
            ];

            // all of the above is being inserted directly into existing YAML,
            // so it has to be indented exactly where it needs to go or it's not valid YAML.
            // aren't whitespace sensitive grammars great?
            let indent_size = 6;
            let execute = execute_lines
                .iter()
                .enumerate()
                .map(|(i, line)| {
                    if i == 0 || line.is_empty() {
                        line.to_string()
                    } else {
                        format!("{:indent$}{}", "", line, indent = indent_size)
                    }
                })
                .collect::<Vec<String>>()
                .join("\n");

            (downloads, execute)
        })
        .unwrap_or((String::new(), String::new()));

    context.insert("custom_cloud_init_downloads".to_string(), download_block);
    context.insert("custom_cloud_init_execute".to_string(), execute_block);

    ("user-data".to_string(), context)
}

pub async fn user_data(machine: Machine, state: State<AppState>) -> impl IntoResponse {
    let (template_key, template_data) = match (
        machine.instructions.custom_cloud_init,
        machine.instructions.discovery_instructions,
    ) {
        (Some(custom_cloud_init), _) => {
            let mut template_data: HashMap<String, String> = HashMap::new();
            template_data.insert("user_data".to_string(), custom_cloud_init);
            ("user-data-assigned".to_string(), template_data)
        }
        (None, Some(discovery_instructions)) => {
            match (
                discovery_instructions.machine_interface,
                discovery_instructions.domain,
            ) {
                (Some(interface), Some(domain)) => match interface.id {
                    Some(machine_interface_id) => {
                        user_data_handler(
                            machine_interface_id,
                            interface,
                            domain,
                            discovery_instructions.hbn_reps,
                            discovery_instructions.hbn_sfs,
                            discovery_instructions.num_of_vfs,
                            discovery_instructions.vf_intercept_bridge_name,
                            discovery_instructions.host_intercept_bridge_name,
                            discovery_instructions.host_intercept_bridge_port,
                            discovery_instructions.vf_intercept_bridge_port,
                            discovery_instructions.vf_intercept_bridge_sf,
                            machine.instructions.api_url_override,
                            machine.instructions.pxe_url_override,
                            state.clone(),
                        )
                        .await
                    }
                    None => print_and_generate_generic_error(format!(
                        "The interface ID should not be null: {interface:?}"
                    )),
                },
                (d, i) => print_and_generate_generic_error(format!(
                    "The interface and domain were not found: {i:?}, {d:?}"
                )),
            }
        }
        (None, None) => {
            // there are two options here
            // 1) this is an allocated instance without custom cloud init, and we should respond with an empty user-data
            // 2) this is the discovery OS on an unallocated instance, and we should respond with that template

            // determine which by trying to parse the instance id in the metadata given to use as a machine id,
            // which is what we will be given for the discovery OS from the api server.

            //TODO: this code was written under teh assumption that hosts were calling this API, and I subsequently realized they are not.  I have to write the routes and the client code for them to even call home first.
            if let Some(_machine_id) = machine
                .instructions
                .metadata
                .map(|m| m.instance_id.parse::<MachineId>().ok())
                .flatten()
            {
                todo!("discovery os cloud init template required");
            } else {
                let mut template_data: HashMap<String, String> = HashMap::new();
                template_data.insert("user_data".to_string(), "{}".to_string());
                ("user-data-assigned".to_string(), template_data)
            }
        }
    };

    axum_template::Render(template_key, state.engine.clone(), template_data)
}

pub async fn meta_data(machine: Machine, state: State<AppState>) -> impl IntoResponse {
    let (template_key, template_data) = match machine.instructions.metadata {
        None => print_and_generate_generic_error(format!(
            "No metadata was found for machine {machine:?}"
        )),
        Some(metadata) => {
            let template_data = HashMap::from([
                ("instance_id".to_string(), metadata.instance_id),
                ("cloud_name".to_string(), metadata.cloud_name),
                ("platform".to_string(), metadata.platform),
            ]);

            ("meta-data".to_string(), template_data)
        }
    };

    axum_template::Render(template_key, state.engine.clone(), template_data)
}

pub async fn vendor_data(state: State<AppState>) -> impl IntoResponse {
    axum_template::Render(
        "printcontext",
        state.engine.clone(),
        HashMap::<String, String>::new(),
    )
}

pub fn get_router(path_prefix: &str) -> Router<AppState> {
    Router::new()
        .route(
            format!("{}/{}", path_prefix, "user-data").as_str(),
            get(user_data),
        )
        .route(
            format!("{}/{}", path_prefix, "meta-data").as_str(),
            get(meta_data),
        )
        .route(
            format!("{}/{}", path_prefix, "vendor-data").as_str(),
            get(vendor_data),
        )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    const TEST_DATA_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../pxe/test_data");

    #[test]
    fn forge_agent_config() {
        let interface_id = "91609f10-c91d-470d-a260-6293ea0c1234".parse().unwrap();
        let config = generate_forge_agent_config(interface_id, None);

        // The intent here is to actually test what the written
        // configuration file looks like, so we can visualize to
        // make sure it's going to look like what we think it's
        // supposed to look like. Obviously as various new fields
        // get added to AgentConfig, then our test config will also
        // need to be updated accordingly, but that should be ok.
        let test_config = fs::read_to_string(format!("{TEST_DATA_DIR}/agent_config.toml")).unwrap();
        assert_eq!(config, test_config);

        let data: toml::Value = toml::from_str(&config).unwrap();

        assert_eq!(
            data.get("machine")
                .unwrap()
                .get("interface-id")
                .unwrap()
                .as_str()
                .unwrap(),
            interface_id.to_string().as_str(),
        );

        // No forge-system section when no override is provided.
        assert!(data.get("forge-system").is_none());

        // Check to make sure is_fake_dpu gets skipped
        // from the serialized output.
        let skipped = match data.get("machine").unwrap().get("is_fake_dpu") {
            Some(_val) => false,
            None => true,
        };
        assert!(skipped);
    }

    #[test]
    fn forge_agent_config_with_external_api_url() {
        let interface_id = "91609f10-c91d-470d-a260-6293ea0c1234".parse().unwrap();
        let config = generate_forge_agent_config(interface_id, Some("https://10.99.0.1:1079"));

        let test_config =
            fs::read_to_string(format!("{TEST_DATA_DIR}/agent_config_external.toml")).unwrap();
        assert_eq!(config, test_config);

        let data: toml::Value = toml::from_str(&config).unwrap();

        assert_eq!(
            data.get("forge-system")
                .unwrap()
                .get("api-server")
                .unwrap()
                .as_str()
                .unwrap(),
            "https://10.99.0.1:1079",
        );

        assert_eq!(
            data.get("machine")
                .unwrap()
                .get("interface-id")
                .unwrap()
                .as_str()
                .unwrap(),
            interface_id.to_string().as_str(),
        );
    }

    /// Verifies the real user-data template renders VF settings from the configured count.
    #[test]
    fn user_data_template_uses_configured_num_of_vfs() {
        let template_glob = concat!(env!("CARGO_MANIFEST_DIR"), "/../../pxe/templates/**/*");
        let tera = tera::Tera::new(template_glob).unwrap();

        // Use the same string-valued context shape the route handler passes to Tera.
        let context = HashMap::from([
            (
                "api_url".to_string(),
                "https://carbide-api.forge".to_string(),
            ),
            (
                "forge_agent_config_b64".to_string(),
                "W21hY2hpbmVdCg==".to_string(),
            ),
            ("forge_bmc_fw_update".to_string(), String::new()),
            ("forge_hbn_reps".to_string(), String::new()),
            ("forge_hbn_sfs".to_string(), String::new()),
            (
                "forge_host_intercept_bridge_name".to_string(),
                String::new(),
            ),
            (
                "forge_host_intercept_bridge_port".to_string(),
                String::new(),
            ),
            ("forge_vf_intercept_bridge_name".to_string(), String::new()),
            ("forge_vf_intercept_bridge_port".to_string(), String::new()),
            ("hostname".to_string(), "test-host".to_string()),
            (
                "interface_id".to_string(),
                "91609f10-c91d-470d-a260-6293ea0c1234".to_string(),
            ),
            ("num_of_vfs".to_string(), "3".to_string()),
            (
                "pxe_url".to_string(),
                "http://carbide-pxe.forge".to_string(),
            ),
            ("seconds_since_epoch".to_string(), "0".to_string()),
        ]);
        let rendered = tera
            .render(
                "user-data",
                &tera::Context::from_serialize(context).unwrap(),
            )
            .unwrap();

        // The mlxconfig value and DHCP drop rules should use the configured count.
        assert!(rendered.contains("NUM_OF_VFS=3"));
        assert!(!rendered.contains("NUM_OF_VFS=16"));
        assert_eq!(rendered.matches("--physdev-in pf0vf").count(), 3);
        assert!(rendered.contains("--physdev-in pf0vf0_if"));
        assert!(rendered.contains("--physdev-in pf0vf1_if"));
        assert!(rendered.contains("--physdev-in pf0vf2_if"));
        assert!(!rendered.contains("--physdev-in pf0vf3_if"));
    }
}
