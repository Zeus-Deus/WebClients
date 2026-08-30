import type { AppStatus } from '../../store/app';
import type { SyncState } from '../../store/auth';
import type { Item } from '../db/entities/items';
import { toWasmEntry } from '../entries/items';
import { service } from '../wasm/service';

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
    state: 'ready' | 'locked' | 'needs_login' | 'unavailable' | 'error';
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
    generateCode = service.generate_code,
}: SnapshotInput): HelperSnapshot => {
    const locked = status === 'locked';
    if (status !== 'ready') {
        return {
            state: locked ? 'locked' : 'unavailable',
            locked,
            synced: syncState === 'on',
            account,
            generation,
            now,
            entries: [],
        };
    }

    const entries = items
        .filter((item) => item.syncMetadata?.state !== 'PendingToDelete')
        .sort((a, b) => a.order - b.order)
        .slice(0, 200)
        .flatMap((item): HelperEntry[] => {
            try {
                const codes = generateCode(toWasmEntry(item), BigInt(now));
                const period = Math.max(15, Math.min(120, Math.floor(item.period || 30)));
                return [
                    {
                        id: item.id,
                        name: item.name,
                        issuer: item.issuer,
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
        synced: syncState === 'on',
        account,
        generation,
        now,
        entries,
    };
};
