import { useEffect, useState } from 'react';
import { apiFetchConfig, apiSaveConfig, type FullConfig } from '@/lib/api';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Badge } from '@/components/ui/badge';
import { Separator } from '@/components/ui/separator';
import {
  ChevronDown,
  ChevronRight,
  Copy,
  Eye,
  EyeOff,
  Plus,
  Save,
  X,
  Server,
  Cpu,
  Globe,
  Gauge,
  Key,
  User,
  Shield,
  Tags,
} from 'lucide-react';
import { useTranslation } from 'react-i18next';

function generateApiKey(): string {
  const bytes = new Uint8Array(24);
  crypto.getRandomValues(bytes);
  return 'sk-' + Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
}

/** Khung mục có thể thu gọn */
function Section({
  title,
  icon: Icon,
  defaultOpen = false,
  children,
}: {
  title: string;
  icon: React.ElementType;
  defaultOpen?: boolean;
  children: React.ReactNode;
}) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <Card>
      <CardHeader
        className="cursor-pointer select-none"
        onClick={() => setOpen(!open)}
      >
        <CardTitle className="flex items-center gap-2 text-lg">
          <Icon className="h-5 w-5" />
          {title}
          <span className="ml-auto text-muted-foreground">
            {open ? <ChevronDown className="h-4 w-4" /> : <ChevronRight className="h-4 w-4" />}
          </span>
        </CardTitle>
      </CardHeader>
      {open && <CardContent>{children}</CardContent>}
    </Card>
  );
}

export function ConfigPage() {
  const { t } = useTranslation();
  const [config, setConfig] = useState<FullConfig | null>(null);
  const [saving, setSaving] = useState(false);
  const [message, setMessage] = useState<{ type: 'ok' | 'err'; text: string } | null>(null);
  const [revealedKeys, setRevealedKeys] = useState<Record<number, boolean>>({});
  const [revealedPasswords, setRevealedPasswords] = useState<Record<number, boolean>>({});
  const [oldPassword, setOldPassword] = useState('');
  const [newPassword, setNewPassword] = useState('');

  useEffect(() => {
    apiFetchConfig()
      .then(setConfig)
      .catch(() => setMessage({ type: 'err', text: t('config.loadFailed') }));
  }, [t]);

  if (!config) {
    return <div className="p-4 text-muted-foreground">{t('config.loading')}</div>;
  }

  const update = <T,>(path: string[], value: T) => {
    setConfig((prev) => {
      if (!prev) return prev;
      const next = structuredClone(prev) as unknown as Record<string, unknown>;
      let obj: Record<string, unknown> = next;
      for (let i = 0; i < path.length - 1; i++) {
        obj = obj[path[i]] as Record<string, unknown>;
      }
      obj[path[path.length - 1]] = value as unknown;
      return next as unknown as FullConfig;
    });
};

  const handleSave = async () => {
    setSaving(true);
    setMessage(null);
    try {
      const body: Record<string, unknown> = {
        server: config.server,
        deepseek: config.deepseek,
        context: config.context,
        proxy: config.proxy,
        admin: {
          password_hash: '',
          jwt_secret: '',
          jwt_issued_at: config.admin.jwt_issued_at,
          old_password: oldPassword,
          new_password: newPassword,
        },
        accounts: config.accounts,
        api_keys: config.api_keys.map(k => ({
          key: k.key,
          description: k.description,
        })),
      };
      const res = await apiSaveConfig(body);
      if (res.ok) {
        setMessage({ type: 'ok', text: t('config.saveSuccess') });
        setRevealedKeys({});
        setOldPassword('');
        setNewPassword('');
        const fresh = await apiFetchConfig();
        setConfig(fresh);
      }
    } catch (e: unknown) {
      setMessage({
        type: 'err',
        text: t('config.saveFailedWithReason', { reason: e instanceof Error ? e.message : String(e) }),
      });
    } finally {
      setSaving(false);
    }
  };

  const handleCancel = () => {
    if (confirm(t('config.cancelConfirm'))) {
      setRevealedKeys({});
      apiFetchConfig()
        .then(setConfig)
        .catch(() => setMessage({ type: 'err', text: t('config.loadFailed') }));
    }
  };

  const copyToClipboard = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      // fallback
      const el = document.createElement('textarea');
      el.value = text;
      document.body.appendChild(el);
      el.select();
      document.execCommand('copy');
      document.body.removeChild(el);
    }
  };

  return (
    <div className="space-y-6">
      <h1 className="text-2xl font-bold">{t('config.title')}</h1>

      {message && (
        <div
          className={`p-3 rounded-md text-sm ${
            message.type === 'err' ? 'bg-red-50 text-red-700' : 'bg-green-50 text-green-700'
          }`}
        >
          {message.text}
        </div>
      )}

      {/* ── Accounts (always visible) ──────────────────────────── */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2 text-lg">
            <User className="h-5 w-5" /> {t('config.sections.accounts')}
          </CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          {config.accounts.map((a, i) => (
            <div key={i} className="flex flex-wrap items-end gap-2 p-3 border rounded-md">
              <div className="flex-1 min-w-[120px]">
                <label className="text-xs text-muted-foreground">{t('config.accounts.email')}</label>
                <Input
                  value={a.email}
                  onChange={(e) => {
                    const next = [...config.accounts];
                    next[i] = { ...next[i], email: e.target.value };
                    update(['accounts'], next);
                  }}
                />
              </div>
              <div className="w-24">
                <label className="text-xs text-muted-foreground">{t('config.accounts.mobile')}</label>
                <Input
                  value={a.mobile}
                  onChange={(e) => {
                    const next = [...config.accounts];
                    next[i] = { ...next[i], mobile: e.target.value };
                    update(['accounts'], next);
                  }}
                />
              </div>
              <div className="w-20">
                <label className="text-xs text-muted-foreground">{t('config.accounts.areaCode')}</label>
                <Input
                  value={a.area_code}
                  onChange={(e) => {
                    const next = [...config.accounts];
                    next[i] = { ...next[i], area_code: e.target.value };
                    update(['accounts'], next);
                  }}
                />
              </div>
              <div className="flex-1 min-w-[120px]">
                <label className="text-xs text-muted-foreground">{t('config.accounts.password')}</label>
                <div className="flex items-center gap-1">
                  <Input
                    type={revealedPasswords[i] ? 'text' : 'password'}
                    value={a.password}
                    onChange={(e) => {
                      const next = [...config.accounts];
                      next[i] = { ...next[i], password: e.target.value };
                      update(['accounts'], next);
                    }}
                  />
                  <Button
                    variant="ghost"
                    size="icon"
                    className="shrink-0"
                    onClick={() =>
                      setRevealedPasswords((prev) => ({ ...prev, [i]: !prev[i] }))
                    }
                  >
                    {revealedPasswords[i] ? <EyeOff className="h-4 w-4" /> : <Eye className="h-4 w-4" />}
                  </Button>
                </div>
              </div>
              <Button
                variant="ghost"
                size="icon"
                className="shrink-0"
                onClick={() => update(['accounts'], config.accounts.filter((_, j) => j !== i))}
              >
                <X className="h-4 w-4" />
              </Button>
            </div>
          ))}
          <Button
            variant="outline"
            size="sm"
            onClick={() =>
              update(['accounts'], [
                ...config.accounts,
                { email: '', mobile: '', area_code: '', password: '' },
              ])
            }
          >
            <Plus className="h-4 w-4 mr-1" /> {t('config.accounts.add')}
          </Button>
        </CardContent>
      </Card>

      {/* ── API Keys (always visible) ─────────────────────────── */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2 text-lg">
            <Key className="h-5 w-5" /> {t('config.sections.apiKeys')}
          </CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          {config.api_keys.map((k, i) => (
            <div key={k.key} className="flex items-center gap-2 p-2 border rounded-md">
              {/* Show/hide toggle */}
              <Button
                variant="ghost"
                size="icon"
                className="shrink-0"
                onClick={() =>
                  setRevealedKeys((prev) => ({ ...prev, [i]: !prev[i] }))
                }
              >
                {revealedKeys[i] ? <EyeOff className="h-4 w-4" /> : <Eye className="h-4 w-4" />}
              </Button>
              {/* Key value */}
              <Input
                type={revealedKeys[i] ? 'text' : 'password'}
                value={k.key}
                onChange={(e) => {
                  const next = [...config.api_keys];
                  next[i] = { ...next[i], key: e.target.value };
                  update(['api_keys'], next);
                }}
                className="flex-1 font-mono text-xs"
              />
              {/* Copy */}
              <Button
                variant="ghost"
                size="icon"
                className="shrink-0"
                onClick={() => copyToClipboard(k.key)}
                title={t('config.apiKeys.copyTitle')}
              >
                <Copy className="h-4 w-4" />
              </Button>
              {/* Description */}
              <input
                className="flex-1 min-w-[80px] bg-transparent border-b border-dashed border-muted-foreground/30 text-sm px-1 outline-none focus:border-primary"
                value={k.description}
                placeholder={t('config.apiKeys.placeholder')}
                onChange={(e) => {
                  const next = [...config.api_keys];
                  next[i] = { ...next[i], description: e.target.value };
                  update(['api_keys'], next);
                }}
              />
              {/* Delete */}
              <Button
                variant="ghost"
                size="icon"
                className="shrink-0"
                onClick={() => update(['api_keys'], config.api_keys.filter((_, j) => j !== i))}
              >
                <X className="h-4 w-4" />
              </Button>
            </div>
          ))}
          <Button
            variant="outline"
            size="sm"
            onClick={() => {
              const newKey = generateApiKey();
              update(['api_keys'], [
                ...config.api_keys,
                { key: newKey, description: '' },
              ]);
            }}
          >
            <Plus className="h-4 w-4 mr-1" /> {t('config.apiKeys.add')}
          </Button>
        </CardContent>
      </Card>


      {/* ── Quản trị (thu gọn) ─────────────────────────────── */}
      <Section title={t('config.sections.admin')} icon={Shield}>
        <div className="space-y-3">
          <div className="flex items-center gap-2">
            <Badge variant={config.admin.password_set ? 'default' : 'secondary'}>
              {config.admin.password_set ? t('config.admin.passwordSet') : t('config.admin.passwordNotSet')}
            </Badge>
          </div>
          <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
            <div>
              <label className="text-sm text-muted-foreground block mb-1">{t('config.admin.oldPassword')}</label>
              <Input
                type="password"
                value={oldPassword}
                onChange={(e) => setOldPassword(e.target.value)}
                placeholder={t('config.admin.oldPasswordPlaceholder')}
              />
            </div>
            <div>
              <label className="text-sm text-muted-foreground block mb-1">{t('config.admin.newPassword')}</label>
              <Input
                type="password"
                value={newPassword}
                onChange={(e) => setNewPassword(e.target.value)}
                placeholder={t('config.admin.newPasswordPlaceholder')}
              />
            </div>
          </div>
        </div>
      </Section>
      <Separator className="my-2" />

      {/* ── Server (collapsible) ──────────────────────────────── */}
      <Section title={t('config.sections.server')} icon={Server}>
        <div className="grid grid-cols-1 md:grid-cols-3 gap-4">
          <div>
            <label className="text-sm text-muted-foreground block mb-1">{t('config.server.host')}</label>
            <Input value={config.server.host} onChange={(e) => update(['server', 'host'], e.target.value)} />
          </div>
          <div>
            <label className="text-sm text-muted-foreground block mb-1">{t('config.server.port')}</label>
            <Input
              type="number"
              value={config.server.port}
              onChange={(e) => update(['server', 'port'], Number(e.target.value))}
            />
          </div>
          <div>
            <label className="text-sm text-muted-foreground block mb-1">{t('config.server.corsOrigins')}</label>
            <Input
              value={config.server.cors_origins.join(', ')}
              onChange={(e) =>
                update(
                  ['server', 'cors_origins'],
                  e.target.value.split(/,\s*/).filter(Boolean),
                )
              }
            />
          </div>
        </div>
      </Section>

      {/* ── DeepSeek (collapsible) ────────────────────────────── */}
      <Section title={t('config.sections.deepseek')} icon={Cpu}>
        <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
          <div>
            <label className="text-sm text-muted-foreground block mb-1">{t('config.deepseek.apiBase')}</label>
            <Input
              value={config.deepseek.api_base}
              onChange={(e) => update(['deepseek', 'api_base'], e.target.value)}
            />
          </div>
          <div>
            <label className="text-sm text-muted-foreground block mb-1">{t('config.deepseek.wasmUrl')}</label>
            <Input
              value={config.deepseek.wasm_url}
              onChange={(e) => update(['deepseek', 'wasm_url'], e.target.value)}
            />
          </div>
          <div>
            <label className="text-sm text-muted-foreground block mb-1">{t('config.deepseek.userAgent')}</label>
            <Input
              value={config.deepseek.user_agent}
              onChange={(e) => update(['deepseek', 'user_agent'], e.target.value)}
            />
          </div>
          <div>
            <label className="text-sm text-muted-foreground block mb-1">{t('config.deepseek.clientVersion')}</label>
            <Input
              value={config.deepseek.client_version}
              onChange={(e) => update(['deepseek', 'client_version'], e.target.value)}
            />
          </div>
          <div>
            <label className="text-sm text-muted-foreground block mb-1">{t('config.deepseek.clientPlatform')}</label>
            <Input
              value={config.deepseek.client_platform}
              onChange={(e) => update(['deepseek', 'client_platform'], e.target.value)}
            />
          </div>
          <div>
            <label className="text-sm text-muted-foreground block mb-1">{t('config.deepseek.clientLocale')}</label>
            <Input
              value={config.deepseek.client_locale}
              onChange={(e) => update(['deepseek', 'client_locale'], e.target.value)}
            />
          </div>
        </div>
      </Section>

      {/* ── Models (collapsible) ──────────────────────────────── */}
      <Section title={t('config.sections.context')} icon={Gauge}>
        <div className="space-y-4">
          <label className="flex items-center gap-2 text-sm">
            <input
              type="checkbox"
              checked={config.context.enabled}
              onChange={(e) => update(['context', 'enabled'], e.target.checked)}
            />
            {t('config.context.enabled')}
          </label>
          <div className="grid grid-cols-1 md:grid-cols-3 gap-4">
            <div>
              <label className="text-sm text-muted-foreground block mb-1">{t('config.context.prewarmChars')}</label>
              <Input
                type="number"
                value={config.context.prewarm_chars}
                onChange={(e) => update(['context', 'prewarm_chars'], Number(e.target.value))}
              />
            </div>
            <div>
              <label className="text-sm text-muted-foreground block mb-1">{t('config.context.triggerChars')}</label>
              <Input
                type="number"
                value={config.context.trigger_chars}
                onChange={(e) => update(['context', 'trigger_chars'], Number(e.target.value))}
              />
            </div>
            <div>
              <label className="text-sm text-muted-foreground block mb-1">{t('config.context.keepLastMessages')}</label>
              <Input
                type="number"
                value={config.context.keep_last_messages}
                onChange={(e) => update(['context', 'keep_last_messages'], Number(e.target.value))}
              />
            </div>
            <div>
              <label className="text-sm text-muted-foreground block mb-1">{t('config.context.chunkChars')}</label>
              <Input
                type="number"
                value={config.context.chunk_chars}
                onChange={(e) => update(['context', 'chunk_chars'], Number(e.target.value))}
              />
            </div>
            <div>
              <label className="text-sm text-muted-foreground block mb-1">{t('config.context.summaryWorkers')}</label>
              <Input
                type="number"
                value={config.context.summary_workers}
                onChange={(e) => update(['context', 'summary_workers'], Number(e.target.value))}
              />
            </div>
            <div>
              <label className="text-sm text-muted-foreground block mb-1">{t('config.context.summaryMaxChars')}</label>
              <Input
                type="number"
                value={config.context.summary_max_chars}
                onChange={(e) => update(['context', 'summary_max_chars'], Number(e.target.value))}
              />
            </div>
            <div>
              <label className="text-sm text-muted-foreground block mb-1">{t('config.context.cacheTtlSecs')}</label>
              <Input
                type="number"
                value={config.context.cache_ttl_secs}
                onChange={(e) => update(['context', 'cache_ttl_secs'], Number(e.target.value))}
              />
            </div>
            <div>
              <label className="text-sm text-muted-foreground block mb-1">{t('config.context.backgroundDelayMs')}</label>
              <Input
                type="number"
                value={config.context.background_delay_ms}
                onChange={(e) => update(['context', 'background_delay_ms'], Number(e.target.value))}
              />
            </div>
            <div>
              <label className="text-sm text-muted-foreground block mb-1">{t('config.context.summaryModelType')}</label>
              <Input
                value={config.context.summary_model_type}
                onChange={(e) => update(['context', 'summary_model_type'], e.target.value)}
              />
            </div>
          </div>
        </div>
      </Section>

      <Section title={t('config.sections.models')} icon={Globe}>
        <div className="space-y-3">
          {config.deepseek.model_types.map((_, i) => (
            <div key={i} className="flex flex-wrap items-end gap-2 p-3 border rounded-md">
              <div className="flex-1 min-w-[120px]">
                <label className="text-xs text-muted-foreground">{t('config.modelsSection.typeName')}</label>
                <Input
                  value={config.deepseek.model_types[i]}
                  onChange={(e) => {
                    const next = [...config.deepseek.model_types];
                    next[i] = e.target.value;
                    update(['deepseek', 'model_types'], next);
                  }}
                />
              </div>
              <div className="w-20">
                <label className="text-xs text-muted-foreground">{t('config.modelsSection.maxInput')}</label>
                <Input
                  type="number"
                  value={config.deepseek.max_input_tokens[i]}
                  onChange={(e) => {
                    const next = [...config.deepseek.max_input_tokens];
                    next[i] = Number(e.target.value);
                    update(['deepseek', 'max_input_tokens'], next);
                  }}
                />
              </div>
              <div className="w-20">
                <label className="text-xs text-muted-foreground">{t('config.modelsSection.maxOutput')}</label>
                <Input
                  type="number"
                  value={config.deepseek.max_output_tokens[i]}
                  onChange={(e) => {
                    const next = [...config.deepseek.max_output_tokens];
                    next[i] = Number(e.target.value);
                    update(['deepseek', 'max_output_tokens'], next);
                  }}
                />
              </div>
              <div className="w-24">
                <label className="text-xs text-muted-foreground">{t('config.modelsSection.inputCharLimit')}</label>
                <Input
                  type="number"
                  value={config.deepseek.input_character_limits[i]}
                  onChange={(e) => {
                    const next = [...config.deepseek.input_character_limits];
                    next[i] = Number(e.target.value);
                    update(['deepseek', 'input_character_limits'], next);
                  }}
                />
              </div>
              <div className="flex-1 min-w-[120px]">
                <label className="text-xs text-muted-foreground">{t('config.modelsSection.alias')}</label>
                <Input
                  value={config.deepseek.model_aliases[i] || ''}
                  onChange={(e) => {
                    const next = [...config.deepseek.model_aliases];
                    next[i] = e.target.value;
                    update(['deepseek', 'model_aliases'], next);
                  }}
                />
              </div>
              <Button
                variant="ghost"
                size="icon"
                className="shrink-0"
                onClick={() => {
                  update(['deepseek', 'model_types'], config.deepseek.model_types.filter((_, j) => j !== i));
                  update(['deepseek', 'max_input_tokens'], config.deepseek.max_input_tokens.filter((_, j) => j !== i));
                  update(
                    ['deepseek', 'max_output_tokens'],
                    config.deepseek.max_output_tokens.filter((_, j) => j !== i),
                  );
                  update(
                    ['deepseek', 'input_character_limits'],
                    config.deepseek.input_character_limits.filter((_, j) => j !== i),
                  );
                  update(['deepseek', 'model_aliases'], config.deepseek.model_aliases.filter((_, j) => j !== i));
                }}
              >
                <X className="h-4 w-4" />
              </Button>
            </div>
          ))}
          <Button
            variant="outline"
            size="sm"
            onClick={() => {
              update(['deepseek', 'model_types'], [...config.deepseek.model_types, 'new']);
              update(['deepseek', 'max_input_tokens'], [...config.deepseek.max_input_tokens, 32000]);
              update(['deepseek', 'max_output_tokens'], [...config.deepseek.max_output_tokens, 8000]);
              update(['deepseek', 'input_character_limits'], [...config.deepseek.input_character_limits, 2621440]);
              update(['deepseek', 'model_aliases'], [...config.deepseek.model_aliases, '']);
            }}
          >
            <Plus className="h-4 w-4 mr-1" /> {t('config.modelsSection.add')}
          </Button>
        </div>
      </Section>

      {/* ── Tool Call Tags (collapsible) ──────────────────────── */}
      <Section title={t('config.sections.toolCallTags')} icon={Tags}>
        <div className="space-y-4">
          <div>
            <label className="text-sm text-muted-foreground block mb-1">{t('config.toolCallTags.extraStarts')}</label>
            <div className="flex flex-wrap gap-2">
              {config.deepseek.tool_call.extra_starts.map((tag, i) => (
                <Badge key={i} variant="secondary" className="gap-1">
                  {tag}
                  <button
                    onClick={() => {
                      const next = config.deepseek.tool_call.extra_starts.filter((_, j) => j !== i);
                      update(['deepseek', 'tool_call', 'extra_starts'], next);
                    }}
                  >
                    <X className="h-3 w-3" />
                  </button>
                </Badge>
              ))}
              <Input
                className="w-48 h-8 text-xs"
                placeholder={t('config.toolCallTags.placeholder')}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' && e.currentTarget.value.trim()) {
                    update(['deepseek', 'tool_call', 'extra_starts'], [
                      ...config.deepseek.tool_call.extra_starts,
                      e.currentTarget.value.trim(),
                    ]);
                    e.currentTarget.value = '';
                  }
                }}
              />
            </div>
          </div>
          <div>
            <label className="text-sm text-muted-foreground block mb-1">{t('config.toolCallTags.extraEnds')}</label>
            <div className="flex flex-wrap gap-2">
              {config.deepseek.tool_call.extra_ends.map((tag, i) => (
                <Badge key={i} variant="secondary" className="gap-1">
                  {tag}
                  <button
                    onClick={() => {
                      const next = config.deepseek.tool_call.extra_ends.filter((_, j) => j !== i);
                      update(['deepseek', 'tool_call', 'extra_ends'], next);
                    }}
                  >
                    <X className="h-3 w-3" />
                  </button>
                </Badge>
              ))}
              <Input
                className="w-48 h-8 text-xs"
                placeholder={t('config.toolCallTags.placeholder')}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' && e.currentTarget.value.trim()) {
                    update(['deepseek', 'tool_call', 'extra_ends'], [
                      ...config.deepseek.tool_call.extra_ends,
                      e.currentTarget.value.trim(),
                    ]);
                    e.currentTarget.value = '';
                  }
                }}
              />
            </div>
          </div>
        </div>
      </Section>

      {/* ── Proxy (collapsible) ───────────────────────────────── */}
      <Section title={t('config.sections.proxy')} icon={Globe}>
        <div>
            <label className="text-sm text-muted-foreground block mb-1">{t('config.proxy.url')}</label>
          <Input
            value={config.proxy.url || ''}
            placeholder={t('config.proxy.placeholder')}
            onChange={(e) => update(['proxy', 'url'], e.target.value || null)}
          />
        </div>
      </Section>

      <Separator className="my-2" />

      {/* ── Action buttons ────────────────────────────────────── */}
      <div className="flex justify-end gap-3">
        <Button variant="outline" onClick={handleCancel} disabled={saving}>
          {t('config.cancel')}
        </Button>
        <Button onClick={handleSave} disabled={saving}>
          <Save className="h-5 w-5 mr-2" />
          {saving ? t('config.saving') : t('config.save')}
        </Button>
      </div>
    </div>
  );
}
