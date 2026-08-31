import fs from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const QA_MARKER = 'qa::';

export function verifyLatestFingerprint(root) {
    const candidates = [];
    for (const entry of fs.readdirSync(root, { withFileTypes: true })) {
        if (!entry.isDirectory() || !entry.name.startsWith('proton-authenticator-')) continue;
        const file = path.join(root, entry.name, 'bin-proton-authenticator.json');
        if (!fs.existsSync(file)) continue;
        candidates.push({ file, mtimeMs: fs.statSync(file).mtimeMs });
    }
    if (candidates.length === 0) throw new Error('helper Cargo fingerprint missing');
    candidates.sort((a, b) => b.mtimeMs - a.mtimeMs || a.file.localeCompare(b.file));
    const latest = candidates[0].file;
    const fingerprint = JSON.parse(fs.readFileSync(latest, 'utf8'));
    const features = Array.isArray(fingerprint.features)
        ? fingerprint.features
        : JSON.parse(String(fingerprint.features || '[]'));
    if (features.includes('devtools')) {
        throw new Error(`refusing helper fingerprint with devtools: ${latest}`);
    }
    return latest;
}

function walkFiles(root) {
    const files = [];
    const pending = [root];
    while (pending.length > 0) {
        const current = pending.pop();
        for (const entry of fs.readdirSync(current, { withFileTypes: true })) {
            const file = path.join(current, entry.name);
            if (entry.isDirectory()) pending.push(file);
            else if (entry.isFile()) files.push(file);
        }
    }
    return files;
}

/** The bundle is embedded verbatim in the release binary, so anything left in
 * `dist` ships to the user: source maps expose the unminified sources and any
 * `qa::` hook means the build was made with QA features enabled. */
export function verifyDistBundle(dist) {
    if (!fs.existsSync(dist)) throw new Error(`helper web bundle missing: ${dist}`);
    const files = walkFiles(dist);
    const maps = files.filter((file) => file.endsWith('.map'));
    if (maps.length > 0) {
        throw new Error(`refusing helper bundle with source maps: ${maps.join(', ')}`);
    }
    const scripts = files.filter((file) => file.endsWith('.js'));
    if (scripts.length === 0) throw new Error(`helper bundle has no scripts: ${dist}`);
    const tainted = scripts.filter((file) => fs.readFileSync(file, 'utf8').includes(QA_MARKER));
    if (tainted.length > 0) {
        throw new Error(`refusing helper bundle with QA hooks: ${tainted.join(', ')}`);
    }
    return scripts.length;
}

/** `dist` being clean is not sufficient: Tauri's codegen keeps a compressed
 * asset cache under the Cargo `OUT_DIR` that Cargo never prunes, so blobs from
 * earlier QA-enabled builds survive `rm -rf dist`. They only stay out of the
 * binary while `dist` does not reference them, which is a property of the last
 * build rather than a guarantee. Assert the shipped binary embeds exactly the
 * bundles the current `dist` declares. */
export function verifyEmbeddedBundles(binary, dist) {
    if (!fs.existsSync(binary)) throw new Error(`helper binary missing: ${binary}`);
    const expected = new Set(
        walkFiles(dist)
            .filter((file) => file.endsWith('.js'))
            .map((file) => path.basename(file))
    );
    const contents = fs.readFileSync(binary, 'latin1');
    const embedded = new Set(contents.match(/main\.[0-9a-f]{6,}\.js/g) ?? []);
    const strays = [...embedded].filter((name) => !expected.has(name));
    if (strays.length > 0) {
        throw new Error(`refusing helper binary embedding stale bundles: ${strays.join(', ')}`);
    }
    const sourceMaps = new Set(contents.match(/[\w.-]+\.js\.map/g) ?? []);
    if (sourceMaps.size > 0) {
        throw new Error(`refusing helper binary embedding source maps: ${[...sourceMaps].join(', ')}`);
    }
    return embedded.size;
}


if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
    const [root, dist, binary] = process.argv.slice(2);
    if (!root || !dist) throw new Error('usage: verify-omarchy-helper-build.mjs <fingerprint-root> <dist> [binary]');
    process.stdout.write(`${verifyLatestFingerprint(path.resolve(root))}\n`);
    process.stdout.write(`${verifyDistBundle(path.resolve(dist))} bundle scripts verified\n`);
    if (binary) {
        const embedded = verifyEmbeddedBundles(path.resolve(binary), path.resolve(dist));
        process.stdout.write(`${embedded} embedded bundle(s) verified\n`);
    }
}
