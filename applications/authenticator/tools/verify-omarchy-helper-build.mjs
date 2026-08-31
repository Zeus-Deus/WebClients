import fs from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

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

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
    const root = process.argv[2];
    if (!root) throw new Error('usage: verify-omarchy-helper-build.mjs <fingerprint-root>');
    process.stdout.write(`${verifyLatestFingerprint(path.resolve(root))}\n`);
}
