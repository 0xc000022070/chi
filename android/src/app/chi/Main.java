package app.chi;

import android.app.Activity;
import android.app.DownloadManager;
import android.content.Intent;
import android.graphics.Bitmap;
import android.graphics.Typeface;
import android.net.Uri;
import android.os.Bundle;
import android.os.Environment;
import android.util.TypedValue;
import android.view.Gravity;
import android.view.View;
import android.view.WindowInsets;
import android.webkit.CookieManager;
import android.webkit.URLUtil;
import android.webkit.WebChromeClient;
import android.webkit.WebResourceError;
import android.webkit.WebResourceRequest;
import android.webkit.WebSettings;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import android.widget.FrameLayout;
import android.widget.LinearLayout;
import android.widget.ProgressBar;
import android.widget.TextView;
import android.widget.Toast;
import android.window.OnBackInvokedCallback;
import android.window.OnBackInvokedDispatcher;
import java.util.List;

public final class Main extends Activity {
    private static final String HOST = "chi.nshard.com";
    private static final String HOME = "https://" + HOST;
    private static final String[] TABS = {"bugs", "builds", "settings"};
    private static final String[] GLYPHS = {"[!]", "[#]", "[~]"};
    private static final List<String> RESERVED = List.of("api", "login", "logout", "orgs", "invite", "sso", "callback");

    private final TextView[][] labels = new TextView[TABS.length][];
    private WebView web;
    private final OnBackInvokedCallback back = () -> web.goBack();
    private ProgressBar progress;
    private TextView offline;
    private LinearLayout dock;
    private String slug;
    private boolean backHooked;
    private boolean typing;

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        web = new WebView(this);
        WebSettings settings = web.getSettings();
        settings.setJavaScriptEnabled(true);
        settings.setDomStorageEnabled(true);
        settings.setUserAgentString(settings.getUserAgentString() + " chi-app");
        web.setBackgroundColor(getColor(R.color.bg));
        web.setWebViewClient(new Pages());
        web.setWebChromeClient(new WebChromeClient() {
            @Override
            public void onProgressChanged(WebView view, int value) {
                progress.setProgress(value);
                progress.setVisibility(value < 100 ? View.VISIBLE : View.GONE);
            }
        });
        web.setDownloadListener((url, agent, disposition, mime, length) -> download(url, agent, disposition, mime));

        progress = new ProgressBar(this, null, android.R.attr.progressBarStyleHorizontal);
        progress.setVisibility(View.GONE);

        offline = text("offline\n\ntap to retry", 16, Typeface.MONOSPACE, R.color.muted);
        offline.setBackgroundColor(getColor(R.color.bg));
        offline.setVisibility(View.GONE);
        offline.setOnClickListener(v -> {
            offline.setVisibility(View.GONE);
            web.reload();
        });

        FrameLayout stage = new FrameLayout(this);
        stage.addView(web, new FrameLayout.LayoutParams(-1, -1));
        stage.addView(progress, new FrameLayout.LayoutParams(-1, dp(3), Gravity.TOP));
        stage.addView(offline, new FrameLayout.LayoutParams(-1, -1));

        LinearLayout bar = new LinearLayout(this);
        for (int i = 0; i < TABS.length; i++) {
            bar.addView(tab(i), new LinearLayout.LayoutParams(0, dp(56), 1));
        }
        View line = new View(this);
        line.setBackgroundColor(getColor(R.color.line));
        dock = new LinearLayout(this);
        dock.setVisibility(View.GONE);
        dock.setOrientation(LinearLayout.VERTICAL);
        dock.addView(line, new LinearLayout.LayoutParams(-1, dp(1)));
        dock.addView(bar, new LinearLayout.LayoutParams(-1, -2));

        LinearLayout root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        root.setBackgroundColor(getColor(R.color.bg));
        root.addView(stage, new LinearLayout.LayoutParams(-1, 0, 1));
        root.addView(dock, new LinearLayout.LayoutParams(-1, -2));
        root.setOnApplyWindowInsetsListener((view, insets) -> {
            int edges = WindowInsets.Type.systemBars() | WindowInsets.Type.displayCutout() | WindowInsets.Type.ime();
            android.graphics.Insets box = insets.getInsets(edges);
            view.setPadding(box.left, box.top, box.right, box.bottom);
            typing = insets.isVisible(WindowInsets.Type.ime());
            showBar();
            return WindowInsets.CONSUMED;
        });
        setContentView(root);

        if (state == null || web.restoreState(state) == null) {
            web.loadUrl(getPreferences(MODE_PRIVATE).getString("last", HOME));
        }
    }

    @Override
    protected void onSaveInstanceState(Bundle state) {
        super.onSaveInstanceState(state);
        web.saveState(state);
    }

    private int dp(int value) {
        return Math.round(value * getResources().getDisplayMetrics().density);
    }

    private TextView text(String value, int size, Typeface face, int color) {
        TextView view = new TextView(this);
        view.setText(value);
        view.setTextSize(size);
        view.setTypeface(face);
        view.setTextColor(getColor(color));
        view.setGravity(Gravity.CENTER);
        return view;
    }

    private View tab(int index) {
        TextView glyph = text(GLYPHS[index], 13, Typeface.create(Typeface.MONOSPACE, Typeface.BOLD), R.color.muted);
        TextView name = text(TABS[index], 12, Typeface.DEFAULT, R.color.muted);
        labels[index] = new TextView[] {glyph, name};

        TypedValue ripple = new TypedValue();
        getTheme().resolveAttribute(android.R.attr.selectableItemBackgroundBorderless, ripple, true);

        LinearLayout tab = new LinearLayout(this);
        tab.setOrientation(LinearLayout.VERTICAL);
        tab.setGravity(Gravity.CENTER);
        tab.addView(glyph);
        tab.addView(name);
        tab.setContentDescription(TABS[index]);
        tab.setBackgroundResource(ripple.resourceId);
        tab.setOnClickListener(v -> web.loadUrl(HOME + "/" + slug + "/" + TABS[index]));
        return tab;
    }

    private void showBar() {
        dock.setVisibility(slug != null && !typing ? View.VISIBLE : View.GONE);
    }

    private void sync(String url) {
        Uri uri = Uri.parse(url);
        List<String> path = uri.getPathSegments();
        boolean org = HOST.equals(uri.getHost()) && !path.isEmpty() && !RESERVED.contains(path.get(0));
        slug = org ? path.get(0) : null;
        if (org) {
            getPreferences(MODE_PRIVATE).edit().putString("last", url).apply();
        }
        String section = org && path.size() > 1 ? path.get(1) : "";
        for (int i = 0; i < TABS.length; i++) {
            boolean active = TABS[i].equals(section);
            labels[i][0].setTextColor(getColor(active ? R.color.accent : R.color.muted));
            labels[i][1].setTextColor(getColor(active ? R.color.ink : R.color.muted));
        }
        showBar();

        OnBackInvokedDispatcher dispatcher = getOnBackInvokedDispatcher();
        if (web.canGoBack() != backHooked) {
            backHooked = !backHooked;
            if (backHooked) {
                dispatcher.registerOnBackInvokedCallback(OnBackInvokedDispatcher.PRIORITY_DEFAULT, back);
            } else {
                dispatcher.unregisterOnBackInvokedCallback(back);
            }
        }
    }

    private void download(String url, String agent, String disposition, String mime) {
        String name = URLUtil.guessFileName(url, disposition, mime);
        String cookie = CookieManager.getInstance().getCookie(url);
        DownloadManager.Request request = new DownloadManager.Request(Uri.parse(url));
        request.addRequestHeader("User-Agent", agent);
        if (cookie != null) {
            request.addRequestHeader("Cookie", cookie);
        }
        request.setTitle(name);
        request.setMimeType(mime);
        request.setNotificationVisibility(DownloadManager.Request.VISIBILITY_VISIBLE_NOTIFY_COMPLETED);
        request.setDestinationInExternalPublicDir(Environment.DIRECTORY_DOWNLOADS, name);
        getSystemService(DownloadManager.class).enqueue(request);
        Toast.makeText(this, "downloading " + name, Toast.LENGTH_SHORT).show();
    }

    private final class Pages extends WebViewClient {
        @Override
        public boolean shouldOverrideUrlLoading(WebView view, WebResourceRequest request) {
            String scheme = request.getUrl().getScheme();
            if ("https".equals(scheme) || "http".equals(scheme)) {
                return false;
            }
            try {
                startActivity(new Intent(Intent.ACTION_VIEW, request.getUrl()));
            } catch (RuntimeException missing) {
                Toast.makeText(Main.this, "nothing opens " + scheme + " links", Toast.LENGTH_SHORT).show();
            }
            return true;
        }

        @Override
        public void onPageStarted(WebView view, String url, Bitmap icon) {
            offline.setVisibility(View.GONE);
        }

        @Override
        public void doUpdateVisitedHistory(WebView view, String url, boolean reload) {
            sync(url);
        }

        @Override
        public void onReceivedError(WebView view, WebResourceRequest request, WebResourceError error) {
            if (request.isForMainFrame()) {
                offline.setVisibility(View.VISIBLE);
            }
        }
    }
}
