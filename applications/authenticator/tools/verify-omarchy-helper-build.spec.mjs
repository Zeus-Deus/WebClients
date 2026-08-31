import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';

import { verifyLatestFingerprint } from './verify-omarchy-helper-build.mjs';

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
