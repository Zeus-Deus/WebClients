import { type FC, useEffect, useState } from 'react';

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

import { shouldEnableHelper } from '../../lib/helper/snapshot';
import logger from '../../lib/logger';
import runtime from '../../lib/tauri/runtime';
import { ProtonSyncModal } from './Settings/Sync/ProtonSyncModal';

const LOGIN_EVENT = 'omarchy-helper:login';
const ADD_EVENT = 'omarchy-helper:add';
const TAKE_LOGIN_REQUEST = 'take_helper_login_request';

type Props = { onAddRequested?: () => void };

/** Surfaces Proton's own UI when the Omarchy panel asks for it: the Device sync
 * sign-in modal (`login`) or the add-code dialog (`add`). Nothing here reads or
 * forwards credentials; the user types them into Proton's own components. */
export const HelperLoginBridge: FC<Props> = ({ onAddRequested }) => {
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

        let disposeAdd: undefined | (() => void);
        void listen(ADD_EVENT, () => {
            if (active) onAddRequested?.();
        })
            .then((unlisten) => {
                if (!active) unlisten();
                else disposeAdd = unlisten;
            })
            .catch(() => {
                logger.error('[omarchy-helper] could not initialize add bridge');
            });

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
            disposeAdd?.();
        };
    }, [enabled, onAddRequested]);

    return open ? <ProtonSyncModal onClose={() => setOpen(false)} /> : null;
};
