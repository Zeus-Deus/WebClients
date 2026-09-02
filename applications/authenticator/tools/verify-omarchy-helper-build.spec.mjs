import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';

import { verifyDistBundle, verifyEmbeddedBundles, verifyEmbeddedCommit, verifyLatestFingerprint } from './verify-omarchy-helper-build.mjs';

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

function writeBinary(contents) {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'helper-binary-'));
    const file = path.join(dir, 'proton-authenticator');
    fs.writeFileSync(file, contents, 'latin1');
    return file;
}

test('accepts a binary embedding only the current bundle', () => {
    const dist = writeDist({
        'index.html': '<script src="main.0783a71b.js"></script>',
        'assets/static/main.0783a71b.js': 'console.log("ok")',
    });
    const binary = writeBinary('\u0000ELF junk assets/static/main.0783a71b.js more junk');
    assert.equal(verifyEmbeddedBundles(binary, dist), 1);
    fs.rmSync(dist, { recursive: true });
    fs.rmSync(path.dirname(binary), { recursive: true });
});

test('rejects a binary embedding bundles that are no longer in dist', () => {
    const dist = writeDist({ 'assets/static/main.0783a71b.js': 'console.log("ok")' });
    const binary = writeBinary('main.0783a71b.js and the stale main.802de1c3.js');
    assert.throws(() => verifyEmbeddedBundles(binary, dist), /stale bundles: main\.802de1c3\.js/);
    fs.rmSync(dist, { recursive: true });
    fs.rmSync(path.dirname(binary), { recursive: true });
});

test('rejects a binary embedding source maps', () => {
    const dist = writeDist({ 'assets/static/main.0783a71b.js': 'console.log("ok")' });
    const binary = writeBinary('main.0783a71b.js plus main.0783a71b.js.map');
    assert.throws(() => verifyEmbeddedBundles(binary, dist), /source maps/);
    fs.rmSync(dist, { recursive: true });
    fs.rmSync(path.dirname(binary), { recursive: true });
});

test('rejects a missing binary', () => {
    const dist = writeDist({ 'assets/static/main.0783a71b.js': 'console.log("ok")' });
    const binary = path.join(os.tmpdir(), `helper-binary-missing-${process.pid}`);
    assert.throws(() => verifyEmbeddedBundles(binary, dist), /binary missing/);
    fs.rmSync(dist, { recursive: true });
});

const COMMIT = 'a'.repeat(40);

test('accepts a binary embedding the exact source commit', () => {
    const binary = writeBinary(`ELF ${COMMIT} tail`);
    assert.equal(verifyEmbeddedCommit(binary, COMMIT), COMMIT);
    fs.rmSync(path.dirname(binary), { recursive: true });
});

test('rejects a binary built from another commit or a dirty tree', () => {
    const other = writeBinary(`ELF ${'b'.repeat(40)} tail`);
    assert.throws(() => verifyEmbeddedCommit(other, COMMIT), /does not embed source commit/);
    fs.rmSync(path.dirname(other), { recursive: true });

    const dirty = writeBinary(`ELF ${COMMIT}-dirty tail`);
    assert.throws(() => verifyEmbeddedCommit(dirty, COMMIT), /dirty tree/);
    fs.rmSync(path.dirname(dirty), { recursive: true });

    const binary = writeBinary(`ELF ${COMMIT}`);
    assert.throws(() => verifyEmbeddedCommit(binary, 'HEAD'), /invalid source commit/);
    fs.rmSync(path.dirname(binary), { recursive: true });
});
