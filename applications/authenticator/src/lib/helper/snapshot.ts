import type { AppStatus } from '../../store/app';
import type { SyncState } from '../../store/auth';
import type { Item } from '../db/entities/items';
import { toWasmEntry } from '../entries/items';
import { service } from '../wasm/service';

const INVALID_TEXT = /[\u0000-\u001F\u007F-\u009F\u061C\u200B-\u200F\u202A-\u202E\u2060\u2066-\u2069\uFEFF]/gu;
const VALID_ID = /^[A-Za-z0-9._:-]{1,128}$/;
const encoder = new TextEncoder();

export type HelperState = 'ready' | 'locked' | 'needs_login' | 'unavailable' | 'error';

export const shouldEnableHelper = (isTauri: boolean, platformIsLinux: boolean, userAgent: string): boolean =>
    isTauri && (platformIsLinux || /\blinux\b/i.test(userAgent));

export const shouldQueryHelperItems = (enabled: boolean, status: AppStatus, databaseOpen: boolean): boolean =>
    enabled && status === 'ready' && databaseOpen;

export const sanitizeHelperText = (value: string, maxBytes: number): string => {
    const cleaned = String(value ?? '').replace(INVALID_TEXT, '').trim();
    let result = '';
    let bytes = 0;
    for (const character of cleaned) {
        const size = encoder.encode(character).byteLength;
        if (bytes + size > maxBytes) break;
        result += character;
        bytes += size;
    }
    return result;
};

export type HelperEntry = {
    id: string;
    name: string;
    issuer: string;
    type: 'Totp' | 'Steam';
    code: string;
    nextCode: string;
    period: number;
    validUntil: number;
};

export type HelperSnapshot = {
    state: HelperState;
    locked: boolean;
    synced: boolean;
    account: string;
    generation: number;
    now: number;
    entries: HelperEntry[];
};

type GenerateCode = (
    item: ReturnType<typeof toWasmEntry>,
    now: bigint
) => { current_code: string; next_code: string };

type SnapshotInput = {
    items: Item[];
    status: AppStatus;
    syncState: SyncState;
    account: string;
    generation: number;
    now: number;
    generateCode?: GenerateCode;
};

export const buildHelperSnapshot = ({
    items,
    status,
    syncState,
    account,
    generation,
    now,
    generateCode,
}: SnapshotInput): HelperSnapshot => {
    const locked = status === 'locked';
    const synced = syncState !== 'off';
    const safeAccount = sanitizeHelperText(account, 120);
    if (status !== 'ready') {
        return {
            state: locked ? 'locked' : 'unavailable',
            locked,
            synced,
            account: safeAccount,
            generation,
            now,
            entries: [],
        };
    }

    const codeGenerator = generateCode ?? service.generate_code;
    const entries = items
        .filter((item) => item.syncMetadata?.state !== 'PendingToDelete')
        .sort((a, b) => a.order - b.order)
        .slice(0, 200)
        .flatMap((item): HelperEntry[] => {
            try {
                if (!VALID_ID.test(item.id) || !['Totp', 'Steam'].includes(item.entryType)) return [];
                const codes = codeGenerator(toWasmEntry(item), BigInt(now));
                const period = Math.max(15, Math.min(120, Math.floor(item.period || 30)));
                return [
                    {
                        id: item.id,
                        name: sanitizeHelperText(item.name, 80),
                        issuer: sanitizeHelperText(item.issuer, 80),
                        type: item.entryType,
                        code: codes.current_code,
                        nextCode: codes.next_code,
                        period,
                        validUntil: now - (now % period) + period,
                    },
                ];
            } catch {
                return [];
            }
        });

    return {
        state: 'ready',
        locked: false,
        synced,
        account: safeAccount,
        generation,
        now,
        entries,
    };
};
