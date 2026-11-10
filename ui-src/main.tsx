import {StrictMode, useEffect, useState} from 'react';
import {createRoot} from 'react-dom/client';
import {Theme} from '@astryxdesign/core/theme';

import '@astryxdesign/core/reset.css';
import '@astryxdesign/core/astryx.css';
import './styles/base.css';
import './styles/shell.css';
import './styles/home.css';
import './styles/clips.css';
import './styles/images.css';
import './styles/viewer.css';
import './styles/settings.css';
import './styles/editor.css';

import {initLocale, subscribeLocale, t} from './lib/i18n';
import {setNativeTrayLabels, setNativeScale} from './lib/env';
import {accentVars, resolveAccent} from './theme/viceTheme';
import {StoreProvider, useStore} from './state/store';
import {PlaybackProvider} from './state/playback';
import {AppFrame} from './components/AppFrame';
import {OverlayRoot} from './components/OverlayPortal';
import {Home} from './screens/Home';
import {Clips} from './screens/Clips';
import {Images} from './screens/Images';
import {Settings} from './screens/Settings';
import {Editor} from './screens/Editor';
import {About} from './screens/About';

function syncNativeTrayLabels(): void {
  setNativeTrayLabels(t('tray.openVice'), t('tray.quitVice'));
}

function App() {
  const {state} = useStore();
  const {accent, customAccent, ready, view} = state;
  // One resolve per render, shared by the theme, the ambient wash and the
  // custom properties, so a custom accent cannot be derived three times.
  const {ramp, theme} = resolveAccent(accent, customAccent);
  const [, retranslate] = useState(0);

  useEffect(() => {
    syncNativeTrayLabels();
    return subscribeLocale(() => {
      retranslate(n => n + 1);
      syncNativeTrayLabels();
    });
  }, []);

  useEffect(() => {
    setNativeScale(Number(state.config?.ui?.scale ?? 1));
  }, [state.config?.ui?.scale]);

  // The boot cover is in index.html so it paints before this bundle parses.
  // It goes once there is real data behind it, not merely once React mounted.
  useEffect(() => {
    if (!ready) return;
    const boot = document.getElementById('boot');
    if (!boot) return;
    boot.classList.add('boot-done');
    const remove = () => boot.remove();
    boot.addEventListener('transitionend', remove, {once: true});
    // A missed transitionend must not leave an invisible cover over the app.
    const failsafe = window.setTimeout(remove, 1200);
    return () => window.clearTimeout(failsafe);
  }, [ready]);

  // On the root, not on .vice-app: body and the boot cover both sit outside
  // that element and would otherwise never see the themed background.
  useEffect(() => {
    const root = document.documentElement;
    const vars = accentVars(ramp);
    for (const [key, value] of Object.entries(vars)) root.style.setProperty(key, value);
  }, [ramp]);

  return (
    <Theme theme={theme} mode="dark">
      <div className="vice-ambient" style={accentVars(ramp)} aria-hidden="true" />
      <PlaybackProvider>
        <div className="vice-app" style={accentVars(ramp)}>
          <OverlayRoot>
          <AppFrame>
            <Screen view={view} />
          </AppFrame>
          </OverlayRoot>
        </div>
      </PlaybackProvider>
    </Theme>
  );
}

function Screen({view}: {view: string}) {
  // Building the clip grid is by far the most expensive screen change in the
  // app: a large library is over a thousand cards. Keep the gallery mounted
  // once it has been opened, so moving between Clips, Home and Settings does
  // not throw the whole grid away and build it again on every visit.
  const [clipsMounted, setClipsMounted] = useState(view === 'clips');
  useEffect(() => {
    if (view === 'clips') setClipsMounted(true);
  }, [view]);

  const known = ['home', 'clips', 'images', 'settings', 'editor'];
  return (
    <>
      {view === 'home' ? <Home /> : null}
      {clipsMounted || view === 'clips' ? (
        <div className="screen-keepalive" hidden={view !== 'clips'}>
          <Clips />
        </div>
      ) : null}
      {view === 'images' ? <Images /> : null}
      {view === 'settings' ? <Settings /> : null}
      {view === 'editor' ? <Editor /> : null}
      {!known.includes(view) ? <About /> : null}
    </>
  );
}

// Before the first render, so no screen paints in English and then switches.
initLocale();

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <StoreProvider>
      <App />
    </StoreProvider>
  </StrictMode>,
);
