import type { Item } from '../db/entities/items';
import {
    buildHelperSnapshot,
    shouldEnableHelper,
    shouldQueryHelperItems,
} from './snapshot';

const item: Item = {
    id: 'fixture-rfc6238',
    name: 'test',
    issuer: 'RFC6238',
    note: '',
    order: 1,
    period: 30,
    secret: 'public-rfc-secret',
    uri: 'otpauth://totp/RFC6238:test',
    entryType: 'Totp',
};

const generateCode = () => ({ current_code: '94287082', next_code: '37359152' });

describe('buildHelperSnapshot', () => {
    it('publishes bounded current and next code rows without secrets', () => {
        const snapshot = buildHelperSnapshot({
            items: [item],
            status: 'ready',
            syncState: 'on',
            account: 'test@example.test',
            generation: 7,
            now: 59,
            generateCode,
        });

        expect(snapshot).toEqual({
            state: 'ready',
            locked: false,
            synced: true,
            account: 'test@example.test',
            generation: 7,
            now: 59,
            entries: [
                {
                    id: 'fixture-rfc6238',
                    name: 'test',
                    issuer: 'RFC6238',
                    type: 'Totp',
                    code: '94287082',
                    nextCode: '37359152',
                    period: 30,
                    validUntil: 60,
                },
            ],
        });
        expect(JSON.stringify(snapshot)).not.toContain(item.secret);
        expect(JSON.stringify(snapshot)).not.toContain(item.uri);
    });

    it('does not access the WASM generator before the app is ready', () => {
        const snapshot = buildHelperSnapshot({
            items: [item],
            status: 'locked',
            syncState: 'off',
            account: '',
            generation: 8,
            now: 60,
        });
        expect(snapshot.state).toBe('locked');
        expect(snapshot.locked).toBe(true);
        expect(snapshot.entries).toEqual([]);
    });

    it.each(['loading', 'error'] as const)('keeps configured sync hidden from the sign-in action while %s', (syncState) => {
        const snapshot = buildHelperSnapshot({
            items: [],
            status: 'ready',
            syncState,
            account: '',
            generation: 1,
            now: 60,
            generateCode,
        });
        expect(snapshot.synced).toBe(true);
    });

    it('sanitizes and UTF-8 byte-bounds account and row labels', () => {
        const snapshot = buildHelperSnapshot({
            items: [
                {
                    ...item,
                    name: `\u061C\u200B\u200E\u200F${'A'.repeat(90)}\u202Eignored`,
                    issuer: 'ü'.repeat(90),
                },
            ],
            status: 'ready',
            syncState: 'on',
            account: `acct\u0000${'ü'.repeat(100)}`,
            generation: 1,
            now: 59,
            generateCode,
        });

        expect(Buffer.byteLength(snapshot.account, 'utf8')).toBeLessThanOrEqual(120);
        expect(Buffer.byteLength(snapshot.entries[0].name, 'utf8')).toBeLessThanOrEqual(80);
        expect(Buffer.byteLength(snapshot.entries[0].issuer, 'utf8')).toBeLessThanOrEqual(80);
        expect(snapshot.account).not.toMatch(/[\u0000-\u001F\u007F-\u009F\u202A-\u202E\u2066-\u2069]/);
        expect(snapshot.entries[0].name).not.toMatch(/[\u061C\u200B-\u200F\u202A-\u202E\u2060\u2066-\u2069\uFEFF]/);
    });
});

describe('shouldEnableHelper', () => {
    it('recognizes WebKitGTK Linux through its user agent fallback', () => {
        expect(shouldEnableHelper(true, false, 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit')).toBe(true);
        expect(shouldEnableHelper(true, true, '')).toBe(true);
        expect(shouldEnableHelper(false, true, 'Linux')).toBe(false);
        expect(shouldEnableHelper(true, false, 'Macintosh')).toBe(false);
    });
});

describe('shouldQueryHelperItems', () => {
    it('waits for the ready app state and an open database', () => {
        expect(shouldQueryHelperItems(true, 'launch', false)).toBe(false);
        expect(shouldQueryHelperItems(true, 'ready', false)).toBe(false);
        expect(shouldQueryHelperItems(true, 'ready', true)).toBe(true);
        expect(shouldQueryHelperItems(false, 'ready', true)).toBe(false);
    });
});
