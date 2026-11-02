// Website analytics for getrad.dev: one anonymous $pageview per page load, nothing
// else. No autocapture, no session recording, no person profile for a visitor who is
// never identified (nobody is). The script runs only on the production hostnames, so a
// local preview or a fork's Pages site sends nothing. This is the website only: the
// radian binary sends no telemetry (ADR 0013).
//
// The PostHog project API key is public by design (it can only write events). An empty
// key, or any value that is not a `phc_` key, turns analytics off and the page is
// unchanged. The project is shared with anylut.com (one project per free plan); tell the
// two sites apart by the `$host` property of each event.
var POSTHOG_KEY = 'phc_ytxuan84PDF95SHcyCaMjW8m9dQW2DoudkUSRQG2aAey'; // gitleaks:allow

var analyticsEnabled =
  POSTHOG_KEY.indexOf('phc_') === 0 &&
  (location.hostname === 'getrad.dev' || location.hostname === 'www.getrad.dev');

if (analyticsEnabled) {
  // Official PostHog CDN snippet: https://posthog.com/docs/libraries/js
  !function(t,e){var o,n,p,r;e.__SV||(window.posthog=e,e._i=[],e.init=function(i,s,a){function g(t,e){var o=e.split(".");2==o.length&&(t=t[o[0]],e=o[1]),t[e]=function(){t.push([e].concat(Array.prototype.slice.call(arguments,0)))}}(p=t.createElement("script")).type="text/javascript",p.crossOrigin="anonymous",p.async=!0,p.src=s.api_host.replace(".i.posthog.com","-assets.i.posthog.com")+"/static/array.js",(r=t.getElementsByTagName("script")[0]).parentNode.insertBefore(p,r);var u=e;for(void 0!==a?u=e[a]=[]:a="posthog",u.people=u.people||[],Object.defineProperty(u,"toString",{configurable:!0,enumerable:!0,writable:!0,value:function(t){var e="posthog";return"posthog"!==a&&(e+="."+a),t||(e+=" (stub)"),e}}),Object.defineProperty(u.people,"toString",{configurable:!0,enumerable:!0,writable:!0,value:function(){return u.toString(1)+".people (stub)"}}),o="init capture register register_once register_for_session unregister unregister_for_session getFeatureFlag getFeatureFlagResult isFeatureEnabled reloadFeatureFlags updateEarlyAccessFeatureEnrollment getEarlyAccessFeatures on onFeatureFlags onSessionId getSurveys getActiveMatchingSurveys renderSurvey canRenderSurvey getNextSurveyStep identify setPersonProperties group resetGroups setPersonPropertiesForFlags resetPersonPropertiesForFlags resetGroupPropertiesForFlags set_config startSessionRecording stopSessionRecording sessionRecordingStarted captureException loadToolbar get_property getSessionProperty createPersonProfile opt_in_capturing opt_out_capturing has_opted_in_capturing has_opted_out_capturing clear_opt_in_out_capturing debug".split(" "),n=0;n<o.length;n++)g(u,o[n]);e._i.push([i,s,a])},e.__SV=1)}(document,window.posthog||[]);
  posthog.init(POSTHOG_KEY, {
    api_host: 'https://eu.i.posthog.com', // the project's cloud region; change both hosts together
    ui_host: 'https://eu.posthog.com',
    defaults: '2026-05-30',
    capture_pageview: true, // plain page loads only; a hash jump is not a pageview
    autocapture: false,
    capture_dead_clicks: false,
    disable_session_recording: true,
    person_profiles: 'identified_only',
  });
}
