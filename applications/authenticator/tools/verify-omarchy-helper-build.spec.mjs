import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';

import { verifyDistBundle, verifyLatestFingerprint } from './verify-omarchy-helper-build.mjs';

function writeFingerprint(root, name, features, mtimeMs) {
    const dir = path.join(root, name);
    fs.mkdirSync(dir, { recursive: true });
    const file = path.join(dir, 'bin-proton-authenticator.json');
    fs.writeFileSync(file, JSON.stringify({ features: JSON.stringify(features) }));
    const time = new Date(mtimeMs);
    fs.utimesSync(file, time, time);
    return file;
}

test('accepts the newest fingerprint when devtools is absent', () => {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), 'helper-fingerprint-'));
    writeFingerprint(root, 'proton-authenticator-old', ['devtools'], 1_000);
    const expected = writeFingerprint(root, 'proton-authenticator-new', [], 2_000);
    assert.equal(verifyLatestFingerprint(root), expected);
    fs.rmSync(root, { recursive: true });
});

test('rejects the newest fingerprint when devtools is present', () => {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), 'helper-fingerprint-'));
    writeFingerprint(root, 'proton-authenticator-old', [], 1_000);
    writeFingerprint(root, 'proton-authenticator-new', ['devtools'], 2_000);
    assert.throws(() => verifyLatestFingerprint(root), /devtools/);
    fs.rmSync(root, { recursive: true });
});

function writeDist(files) {
    const dist = fs.mkdtempSync(path.join(os.tmpdir(), 'helper-dist-'));
    for (const [name, contents] of Object.entries(files)) {
        const file = path.join(dist, name);
        fs.mkdirSync(path.dirname(file), { recursive: true });
        fs.writeFileSync(file, contents);
    }
    return dist;
}

test('accepts a bundle without source maps or QA hooks', () => {
    const dist = writeDist({
        'index.html': '<html></html>',
        'assets/static/main.js': 'console.log("ok")',
    });
    assert.equal(verifyDistBundle(dist), 1);
    fs.rmSync(dist, { recursive: true });
});

test('rejects a bundle shipping source maps', () => {
    const dist = writeDist({
        'assets/static/main.js': 'console.log("ok")',
        'assets/static/main.js.map': '{"version":3}',
    });
    assert.throws(() => verifyDistBundle(dist), /source maps/);
    fs.rmSync(dist, { recursive: true });
});

test('rejects a bundle shipping QA hooks', () => {
    const dist = writeDist({
        'assets/static/main.js': 'window["qa::keyring::suppress"]=1',
    });
    assert.throws(() => verifyDistBundle(dist), /QA hooks/);
    fs.rmSync(dist, { recursive: true });
});

test('rejects a bundle with no scripts at all', () => {
    const dist = writeDist({ 'index.html': '<html></html>' });
    assert.throws(() => verifyDistBundle(dist), /no scripts/);
    fs.rmSync(dist, { recursive: true });
});

test('rejects a missing bundle directory', () => {
    const dist = path.join(os.tmpdir(), `helper-dist-missing-${process.pid}`);
    assert.throws(() => verifyDistBundle(dist), /bundle missing/);
});
