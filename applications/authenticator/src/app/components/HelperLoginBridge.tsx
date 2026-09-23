import { type FC, useEffect, useRef } from 'react';

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { requestFork } from 'proton-authenticator/store/auth';
import { useAppDispatch, useAppSelector } from 'proton-authenticator/store/utils';

import { ForkType } from '@proton/shared/lib/authentication/fork/constants';

import { shouldEnableHelper } from '../../lib/helper/snapshot';
import logger from '../../lib/logger';
import runtime from '../../lib/tauri/runtime';

const LOGIN_EVENT = 'omarchy-helper:login';
const ADD_EVENT = 'omarchy-helper:add';
const TAKE_LOGIN_REQUEST = 'take_helper_login_request';
const SHOW_MAIN_WINDOW = 'show_helper_main_window';

type Props = { onAddRequested?: () => void };

/** Surfaces Proton's own UI when the Omarchy panel asks for it.
 *
 * `login` opens Proton's own sign-in window directly, through the same
 * `requestFork(LOGIN)` call as the app's "Sign in" button, without showing the
 * main window first. When already signed in it shows the main window instead.
 * `add` opens the add-code dialog. Nothing here reads or forwards credentials:
 * the user types them into Proton's hosted sign-in page. */
export const HelperLoginBridge: FC<Props> = ({ onAddRequested }) => {
    const dispatch = useAppDispatch();
    const signedIn = useAppSelector((state) => Boolean(state.auth.session));
    const signedInRef = useRef(signedIn);
    signedInRef.current = signedIn;

    const enabled = shouldEnableHelper(
        runtime.isTauri,
        runtime.platform === 'linux',
        typeof navigator === 'undefined' ? '' : navigator.userAgent
    );

    useEffect(() => {
        if (!enabled) return;
        let active = true;
        let dispose: undefined | (() => void);
        let disposeAdd: undefined | (() => void);

        const startLogin = () => {
            if (!active) return;
            if (signedInRef.current) {
                void invoke(SHOW_MAIN_WINDOW).catch(() => {
                    logger.error('[omarchy-helper] could not show the main window');
                });
                return;
            }
            void dispatch(requestFork(ForkType.LOGIN)).catch(() => {
                logger.error('[omarchy-helper] could not open the sign-in window');
            });
        };

        const consume = () => {
            void invoke<boolean>(TAKE_LOGIN_REQUEST)
                .then((requested) => {
                    if (requested) startLogin();
                })
                .catch(() => {
                    logger.error('[omarchy-helper] could not consume login request');
                });
        };

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
                // A request made before this page loaded (helper started with
                // --login) is still waiting in the one-shot flag.
                consume();
            })
            .catch(() => {
                logger.error('[omarchy-helper] could not initialize login bridge');
            });

        return () => {
            active = false;
            dispose?.();
            disposeAdd?.();
        };
    }, [enabled, onAddRequested, dispatch]);

    return null;
};
