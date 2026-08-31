import { type FC, useEffect, useState } from 'react';

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

import { shouldEnableHelper } from '../../lib/helper/snapshot';
import logger from '../../lib/logger';
import runtime from '../../lib/tauri/runtime';
import { ProtonSyncModal } from './Settings/Sync/ProtonSyncModal';

const LOGIN_EVENT = 'omarchy-helper:login';
const TAKE_LOGIN_REQUEST = 'take_helper_login_request';

export const HelperLoginBridge: FC = () => {
    const [open, setOpen] = useState(false);
    const enabled = shouldEnableHelper(
        runtime.isTauri,
        runtime.platform === 'linux',
        typeof navigator === 'undefined' ? '' : navigator.userAgent
    );

    useEffect(() => {
        if (!enabled) return;
        let active = true;
        let dispose: undefined | (() => void);

        const consume = () => {
            if (active) setOpen(true);
            void invoke<boolean>(TAKE_LOGIN_REQUEST).catch(() => {
                logger.error('[omarchy-helper] could not consume login request');
            });
        };

        void listen(LOGIN_EVENT, consume)
            .then((unlisten) => {
                if (!active) {
                    unlisten();
                    return;
                }
                dispose = unlisten;
                return invoke<boolean>(TAKE_LOGIN_REQUEST);
            })
            .then((requested) => {
                if (active && requested) setOpen(true);
            })
            .catch(() => {
                logger.error('[omarchy-helper] could not initialize login bridge');
            });

        return () => {
            active = false;
            dispose?.();
        };
    }, [enabled]);

    return open ? <ProtonSyncModal onClose={() => setOpen(false)} /> : null;
};
