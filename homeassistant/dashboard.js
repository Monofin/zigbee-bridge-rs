// Only layout lives here. HA owns the cards, service calls and live state.
const tile = (entity, name, options = {}) => ({type: "tile", entity, name, ...options});
const link = (name, icon, path) => ({
  type: "button", name, icon, icon_height: "24px",
  tap_action: {action: "navigate", navigation_path: path},
});
const note = (content) => ({type: "markdown", content, text_only: true});
const section = (title, cards, options = {}) => ({
  title, type: "sections", max_columns: 1, sections: [{type: "grid", cards}], ...options,
});
const hasResult = (entity) => ({
  condition: "state", entity, state_not: ["unknown", "unavailable"],
});

class ZigbeeDashboard extends HTMLElement {
  static async generate(config, hass) {
    const base = config.base_topic ?? "zigbee2mqtt";
    const [entities, devices] = await Promise.all([
      hass.callWS({type: "config/entity_registry/list"}),
      hass.callWS({type: "config/device_registry/list"}),
    ]);
    const mqtt = entities.filter((e) => e.platform === "mqtt");
    const apiKey = "_api_version_" + base;
    const api = mqtt.filter((e) => e.unique_id.endsWith(apiKey)
      && /^bridge_0x[0-9a-f]{16}$/.test(e.unique_id.slice(0, -apiKey.length)) && !e.disabled_by);
    if (api.length !== 1) {
      const message = api.length ? "More than one coordinator advertises this MQTT base. Check the bridge discovery entries."
        : "Waiting for bridge discovery. Check the MQTT base topic and reload this page once the bridge is online.";
      return {views: [section("Network", [note(message)])]};
    }

    const bridge = api[0];
    const prefix = bridge.unique_id.slice(0, -apiKey.length);
    const ids = new Map(mqtt.filter((e) => e.device_id === bridge.device_id && !e.disabled_by)
      .map((e) => [e.unique_id, e.entity_id]));
    const id = (key) => ids.get(prefix + "_" + key + "_" + base);
    const version = bridge.entity_id;
    const supported = [{condition: "state", entity: version, state: "1"}];
    const action = (entity, name, service, confirm) => tile(entity, name, {
      visibility: supported,
      hide_state: true,
      tap_action: {action: "perform-action", perform_action: service, target: {entity_id: entity},
        ...(confirm ? {confirmation: {text: confirm}} : {})},
      icon_tap_action: {action: "none"},
    });
    const network = [
      ...(id("connection") ? [tile(id("connection"), "Bridge connection")] : []),
      {...note("Controls require an online bridge running admin API v1."),
        visibility: [{condition: "state", entity: version, state_not: "1"}]},
    ];
    // Separate commands avoid reversing pairing if another client changes it during a tap.
    const permit = id("permit_join");
    if (permit) {
      for (const [state, label, service, confirm] of [
        ["off", "Open pairing", "switch.turn_on", "Allow devices to join for 254 seconds?"],
        ["on", "Close pairing", "switch.turn_off", null],
      ]) {
        const card = action(permit, label, service, confirm);
        card.visibility = [...supported, {condition: "state", entity: permit, state}];
        network.push(card);
      }
    }
    const dashboard = "/" + location.pathname.split("/")[1];
    const membershipKey = "_linkquality_" + base;
    const inBridge = new Set(mqtt.filter((e) => e.config_entry_id === bridge.config_entry_id
      && e.unique_id.endsWith(membershipKey)
      && /^0x[0-9a-f]{16}$/.test(e.unique_id.slice(0, -membershipKey.length))).map((e) => e.device_id));
    const members = devices.filter((d) => inBridge.has(d.id))
      .sort((a, b) => (a.name_by_user || a.name).localeCompare(b.name_by_user || b.name));
    const pages = [];
    const deviceCards = members.map((d) =>
      link(d.name_by_user || d.name, "mdi:zigbee", "/config/devices/device/" + d.id));
    const deviceViews = [];
    for (let start = 0; start < members.length; start += 8) {
      const path = "devices-" + (start / 8 + 1);
      const batch = members.slice(start, start + 8);
      const label = (d) => d.name_by_user || d.name;
      pages.push(link(label(batch[0]) + " – " + label(batch.at(-1)), "mdi:devices", dashboard + "/" + path));
      deviceViews.push(section("Devices", deviceCards.slice(start, start + 8), {
        path, subview: true, back_path: dashboard + "/devices",
      }));
    }
    const maintenance = [tile(version, "Admin API", {icon: "mdi:api"})];
    if (id("health_check")) maintenance.push(action(id("health_check"), "Check coordinator", "button.press"));
    for (const [key, label] of [["health_result", "Last coordinator check"], ["bind", "Last binding"], ["unbind", "Last unbinding"], ["display_name", "Last display name change"], ["rename_route", "Last MQTT route change"]]) {
      const entity = id(key);
      if (entity) maintenance.push(tile(entity, label, {visibility: [hasResult(entity)]}));
    }
    if (config.binding_script && hass.states[config.binding_script]) {
      maintenance.push(tile(config.binding_script, "Manage bindings", {
        hide_state: true, tap_action: {action: "more-info"}, icon_tap_action: {action: "more-info"}, visibility: supported,
      }));
    }
    if (config.naming_script && hass.states[config.naming_script]) {
      maintenance.push(tile(config.naming_script, "Name a device", {
        hide_state: true, tap_action: {action: "more-info"}, icon_tap_action: {action: "more-info"}, visibility: supported,
      }));
    }
    return {views: [
      section("Network", network, {path: "network"}),
      section("Devices", members.length > 8 ? pages : deviceCards.length ? deviceCards :
        [note("No joined devices have been discovered yet.")], {path: "devices"}),
      section("Maintenance", maintenance, {path: "maintenance"}),
      ...(members.length > 8 ? deviceViews : []),
    ]};
  }
}
customElements.define("ll-strategy-dashboard-zigbee-bridge", ZigbeeDashboard);
