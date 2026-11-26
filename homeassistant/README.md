# Home Assistant dashboard

**Network** shows the broker-reported bridge connection and the pairing control. Opening pairing requires confirmation and uses the bridge's 254-second window. The button changes to **Close pairing** when the coordinator reports it open.

**Devices** opens the native HA device pages, including firmware update entities where the interviewed device advertises OTA. The list uses HA device names and shows up to eight devices per group. Reload the dashboard after adding or removing a device, or after a registry rename.

**Maintenance** shows the advertised admin API major and can ask the adapter whether it is joined. A binding form selects a source, destination and cluster. Its result tile shows the latest response for that operation. Tap it for the transaction, confirmed clusters or error. Running the form sends a request. The radio response supplies the outcome.

## Installation

The dashboard uses Home Assistant's MQTT integration with discovery enabled. Start the managed coordinator with that broker. All paths below are relative to the HA configuration directory.

1. Copy `dashboard.js` to `www/zigbee-dashboard.js`, `dashboard.yaml` to `zigbee-dashboard.yaml`, and both script blueprints (`binding.yaml` and `naming.yaml`) to `blueprints/script/zigbee_bridge/`. Set `base_topic` in the dashboard YAML to the bridge's exact MQTT base.
2. Add the following entries to `configuration.yaml`, merging them with existing entries under the same keys. Restart Home Assistant.

   ```yaml
   frontend:
     extra_module_url:
       - /local/zigbee-dashboard.js
   lovelace:
     dashboards:
       zigbee-admin:
         mode: yaml
         title: Zigbee
         icon: mdi:zigbee
         show_in_sidebar: true
         require_admin: true
         filename: zigbee-dashboard.yaml
   ```

3. In **Settings → Automations & scenes → Blueprints**, create one script from **Zigbee bridge binding** and one from **Zigbee bridge device naming**. Select the coordinator’s **Admin API version** sensor in each. Set `binding_script` and `naming_script` in the dashboard YAML to their saved entity IDs.
4. Open **Zigbee** from the sidebar. Check the connection and run **Maintenance → Check coordinator**. A healthy result means the adapter reported a joined network. The pairing, binding and health controls require an online coordinator advertising admin API v1.

The single frontend module builds native Sections views from HA's registries. It uses the MQTT discovery identities, so an entity rename survives a reload. The script blueprint publishes through the built-in MQTT integration using the base advertised by its selected sensor. Broker ACLs govern access for all clients, including HA. The dashboard requires a Home Assistant administrator.

## Name a joined device

For a local Home Assistant label, use the device page’s rename control. To set the name advertised by the bridge, copy `naming.yaml` to `blueprints/script/zigbee_bridge/naming.yaml`, reload blueprints and create a script from **Zigbee bridge device naming**. Select the bridge’s API sensor and set the dashboard’s `naming_script` to the resulting script entity ID. Open **Maintenance → Name a device** after pairing, select the identified device and leave **Display name** selected. Tap the result tile to see the transaction and returned name or error. An existing Home Assistant name override takes precedence.

Choose **MQTT route** only when you also want to move its application topics. Update any external scripts that use the old route. The discovery identifiers and Home Assistant automations using its existing entities stay associated with the same device. The bridge returns the operation result over its [versioned MQTT API](../REFERENCE.md#device-naming).

## Upgrading the dashboard

Keep the dashboard files with the bridge revision they came from. Save your configured dashboard YAML and script entity IDs. Update the JavaScript module and blueprints, then merge any changes to the supplied dashboard YAML with your MQTT base and script settings. A revision query on the module URL, such as `/local/zigbee-dashboard.js?v=REVISION`, makes browsers request the updated module. Restart HA after changing the frontend configuration and reload open dashboard tabs. Confirm the API major and run the coordinator check.

For rollback, restore the previous assets and URL together. Follow the [bridge rollback procedure](../INSTALL.md#upgrade-and-rollback) for retained discovery. A dashboard installation and its discovered MQTT entities have separate lifetimes.

To remove the dashboard, delete its Lovelace and frontend entries, remove the dashboard scripts and copied files, then restart Home Assistant.
