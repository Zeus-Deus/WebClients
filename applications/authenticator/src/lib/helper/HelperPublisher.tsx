import { type FC, useEffect, useRef, useState } from 'react';

import { invoke } from '@tauri-apps/api/core';
import { useLiveQuery } from 'dexie-react-hooks';

import { useAppSelector } from '../../store/utils';
import { db } from '../db/db';
import runtime from '../tauri/runtime';
import { buildHelperSnapshot } from './snapshot';

/** Publishes bounded in-memory display rows to the Rust Unix-socket helper.
 * Passwords, sessions, user keys, storage keys, and TOTP secrets are never
 * included. Codes are included because the Omarchy panel explicitly displays
 * them; Rust keeps them in memory and copies by opaque item id. */
export const HelperPublisher: FC = () => {
    const enabled = runtime.isTauri && runtime.platform === 'linux';
    const status = useAppSelector((state) => state.app.status);
    const syncState = useAppSelector((state) => state.auth.syncState);
    const account = useAppSelector((state) => state.auth.user?.Email ?? state.auth.user?.Name ?? '');
    const items = useLiveQuery(async () => (enabled && db.isOpen() ? db.items.toSafeArray() : []), [enabled], []);
    const [now, setNow] = useState(() => Math.floor(Date.now() / 1_000));
    const generation = useRef(0);

    useEffect(() => {
        if (!enabled) return;
        const timer = window.setInterval(() => setNow(Math.floor(Date.now() / 1_000)), 1_000);
        return () => window.clearInterval(timer);
    }, [enabled]);

    useEffect(() => {
        if (!enabled) return;
        const snapshot = buildHelperSnapshot({
            items: items ?? [],
            status,
            syncState,
            account,
            generation: ++generation.current,
            now,
        });
        void invoke('publish_helper_snapshot', { snapshot }).catch(() => {});
    }, [enabled, items, status, syncState, account, now]);

    return null;
};
