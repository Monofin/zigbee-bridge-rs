// Offline catalogue intake. Build a clean, pinned zigbee-herdsman-converters
// checkout with its pinned pnpm and frozen lockfile, then run:
//   node tools/devices/z2m.mjs CHECKOUT > z2m.jsonl
// This loads trusted upstream JavaScript including prepareDefinition. Run it
// in a build sandbox with no network, credentials, serial port or broker.
// Generated data retains the upstream MIT notice on redistribution.
import assert from "node:assert/strict";
import {execFileSync} from "node:child_process";
import {readFileSync, readdirSync} from "node:fs";
import {createRequire} from "node:module";
import path from "node:path";

function namesByObject(groups) {
    const names = new Map();
    for (const [prefix, exports] of Object.entries(groups)) {
        for (const [name, value] of Object.entries(exports)) {
            names.set(value, `${prefix}.${name}`);
        }
    }
    return names;
}

function exposeShape(expose) {
    const shape = {};
    for (const key of ["type", "name", "property", "endpoint", "access", "unit", "category",
        "value_min", "value_max", "value_step", "value_on", "value_off", "value_toggle",
        "values", "length_min", "length_max"]) {
        if (expose[key] !== undefined) shape[key] = expose[key];
    }
    if (expose.features) shape.features = expose.features.map(exposeShape);
    if (expose.item_type) shape.item_type = exposeShape(expose.item_type);
    return shape;
}

// Preserve every predicate, including the distinction between a regex and a
// string, and keys matching an explicitly undefined field.
function predicateJSON(_key, value) {
    if (value instanceof RegExp) return {regex: value.source, flags: value.flags};
    if (value === undefined) return {undefined: true};
    if (typeof value === "function") throw new Error("unexpected executable match condition");
    return value;
}

function strings(values) {
    return [...new Set(values.filter(v => typeof v === "string"))].sort();
}

const [checkout, option, ...extra] = process.argv.slice(2);
if (!checkout || (option !== undefined && option !== "--ota-profiles") || extra.length) {
    throw new Error("usage: node z2m.mjs BUILT_CHECKOUT [--ota-profiles]");
}
const otaSelectors = new Map();
function remember(model, selector, supported) {
    if (typeof model !== "string" || !model.length) return;
    if (!otaSelectors.has(model)) otaSelectors.set(model, []);
    otaSelectors.get(model).push({selector, supported});
}
const root = path.resolve(checkout);
const require = createRequire(path.join(root, "package.json"));
const git = (...args) => execFileSync("git", ["-C", root, ...args], {encoding: "utf8"}).trim();
const revision = git("rev-parse", "HEAD");
assert.equal(git("status", "--porcelain", "--", "src", "package.json", "pnpm-lock.yaml"), "",
    "modified upstream sources cannot be attributed to the recorded revision");
assert.equal(require("./package.json").name, "zigbee-herdsman-converters");

const {prepareDefinition} = require("./dist/index.js");
const tuya = require("./dist/lib/tuya.js");
const named = namesByObject({
    fz: require("./dist/converters/fromZigbee.js"),
    tz: require("./dist/converters/toZigbee.js"),
    "tuya.fz": tuya.fz, "tuya.tz": tuya.tz,
    "tuya.valueConverter": tuya.valueConverter,
    "tuya.valueConverterBasic": tuya.valueConverterBasic,
});
const index = JSON.parse(readFileSync(path.join(root, "dist/models-index.json"), "utf8"));
const indexed = new Set(Object.values(index).flat().map(([file, i]) => `${file}:${i}`));
const sources = readdirSync(path.join(root, "src/devices"))
    .filter(f => f.endsWith(".ts") && f !== "index.ts").sort();

for (const file of sources) {
    const js = `${file.slice(0, -3)}.js`;
    const {definitions} = require(`./dist/devices/${js}`);
    for (const [i, raw] of definitions.entries()) {
        const symbol = `definitions[${i}]`;
        assert(indexed.delete(`${js}:${i}`), `${file} ${symbol} missing from generated index`);
        const definition = prepareDefinition(raw);
        const fingerprints = raw.fingerprint ?? [];
        const zigbeeModels = raw.zigbeeModel ?? [];
        if (option === "--ota-profiles") {
            const supported = Boolean(definition.ota);
            for (const model of zigbeeModels) remember(model, {}, supported);
            for (const {modelID, ...selector} of fingerprints) remember(modelID, selector, supported);
            continue;
        }
        // prepareDefinition injects general administrative commands globally.
        // Only inventory outbound converters declared by this device or its extensions.
        const declaredTo = new Set([
            ...(raw.toZigbee ?? []),
            ...(raw.extend ?? []).flatMap(e => e.toZigbee ?? []),
        ]);
        const meta = definition.meta ?? {};
        const row = {
            source: "z2m", revision, path: `src/devices/${file}`, symbol, kind: "definition",
            // Search facets only. Wire matches use the original alternatives below.
            manufacturers: strings(fingerprints.map(f => f.manufacturerName)),
            models: strings([...zigbeeModels, ...fingerprints.map(f => f.modelID)]),
            vendor: raw.vendor, display_model: raw.model,
            zigbee_models: zigbeeModels,
            fingerprints,
            white_labels: (raw.whiteLabel ?? []).map(({vendor, model, fingerprint, whiteLabelOf}) =>
                Object.fromEntries(Object.entries({vendor, model, fingerprint, whiteLabelOf}).filter(([,v]) => v !== undefined))),
            dynamic_exposes: typeof definition.exposes === "function",
            exposes: Array.isArray(definition.exposes) ? definition.exposes.map(exposeShape) : [],
            from_zigbee: definition.fromZigbee.map(c => ({
                symbol: named.get(c) ?? null, cluster: c.cluster, message_types: c.type,
            })),
            to_zigbee: definition.toZigbee.filter(c => declaredTo.has(c)).map(c => ({
                symbol: named.get(c) ?? null, keys: c.key,
                set: typeof c.convertSet === "function", get: typeof c.convertGet === "function",
            })),
            hooks: ["configure", "onEvent", "endpoint"].filter(k => typeof definition[k] === "function"),
            // Functions and closure-created value maps require review at source.
            tuya_datapoints: (meta.tuyaDatapoints ?? []).map(([dp, property, converter, options]) => ({
                dp, property, symbol: named.get(converter) ?? null,
                from: typeof converter?.from === "function", to: typeof converter?.to === "function",
                option_keys: Object.keys(options ?? {}),
            })),
        };
        if (typeof meta.turnsOffAtBrightness1 === "boolean") row.turns_off_at_brightness_1 = meta.turnsOffAtBrightness1;
        // JSON predicates (including aliases) are serialized separately so
        // missing, optional non-matching metadata can be omitted normally.
        row.fingerprints = JSON.parse(JSON.stringify(row.fingerprints, predicateJSON));
        row.white_labels = JSON.parse(JSON.stringify(row.white_labels, predicateJSON));
        process.stdout.write(JSON.stringify(row) + "\n");
    }
}
assert.equal(indexed.size, 0, "generated index references missing definitions");

if (option === "--ota-profiles") {
    // A Basic model/manufacturer pair cannot resolve firmware-version or
    // endpoint-specific exceptions. Omit ambiguous matches from this index.
    const models = [];
    const manufacturers = {};
    for (const model of [...otaSelectors.keys()].sort()) {
        const options = otaSelectors.get(model);
        const defaultOta = options.some(o => o.supported && Object.keys(o.selector).length === 0);
        if (defaultOta && options.every(o => o.supported)) {
            models.push(model);
            continue;
        }
        for (const maker of strings(options.map(o => o.selector.manufacturerName))) {
            const possible = options.filter(o => typeof o.selector.manufacturerName !== "string"
                || o.selector.manufacturerName === maker);
            const proven = defaultOta || possible.some(o => o.supported && o.selector.manufacturerName === maker
                && Object.keys(o.selector).length === 1);
            if (proven && possible.every(o => o.supported)) (manufacturers[maker] ??= []).push(model);
        }
    }
    process.stdout.write(JSON.stringify({revision, models, manufacturers}, null, 2) + "\n");
}
