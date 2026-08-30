import type { Item } from '../db/entities/items';
import { buildHelperSnapshot } from './snapshot';

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

    it('clears all rows while locked', () => {
        const snapshot = buildHelperSnapshot({
            items: [item],
            status: 'locked',
            syncState: 'off',
            account: '',
            generation: 8,
            now: 60,
            generateCode,
        });
        expect(snapshot.state).toBe('locked');
        expect(snapshot.locked).toBe(true);
        expect(snapshot.entries).toEqual([]);
    });
});
