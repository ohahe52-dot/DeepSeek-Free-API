import { useState, useEffect } from 'react';
import { useNavigate } from 'react-router-dom';
import { useAuth } from '@/lib/use-auth';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { KeyRound, Shield } from 'lucide-react';
import { useTranslation } from 'react-i18next';

export function LoginPage() {
  const { t } = useTranslation();
  const { login, setup } = useAuth();
  const navigate = useNavigate();
  const [password, setPassword] = useState('');
  const [confirmPassword, setConfirmPassword] = useState('');
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(false);
  const [needsSetup, setNeedsSetup] = useState<boolean | null>(null);

  // Kiểm tra có cần setup bằng cách thử endpoint đăng nhập.
  useEffect(() => {
    (async () => {
      try {
        const res = await fetch('/admin/api/login', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ password: '__check__' }),
        });
        await res.json().catch(() => ({}));
        // 403 = chưa đặt mật khẩu -> cần setup
        // 401 = sai mật khẩu -> chế độ đăng nhập
        setNeedsSetup(res.status === 403);
      } catch {
        setNeedsSetup(false);
      }
    })();
  }, []);

  const handleSetup = async (e: React.FormEvent) => {
    e.preventDefault();
    setError('');
    if (password.length < 6) {
      setError(t('login.errorPasswordLength'));
      return;
    }
    if (password !== confirmPassword) {
      setError(t('login.errorPasswordMismatch'));
      return;
    }
    setLoading(true);
    const result = await setup(password);
    setLoading(false);
    if (result.success) {
      navigate('/', { replace: true });
    } else {
      setError(result.error || t('login.errorSetupFailed'));
    }
  };

  const handleLogin = async (e: React.FormEvent) => {
    e.preventDefault();
    setError('');
    setLoading(true);
    const result = await login(password);
    setLoading(false);
    if (result.success) {
      navigate('/', { replace: true });
    } else {
      setError(result.error || t('login.errorLoginFailed'));
    }
  };

  if (needsSetup === null) {
    return (
      <div className="min-h-screen flex items-center justify-center bg-background">
        <p className="text-muted-foreground">{t('login.checking')}</p>
      </div>
    );
  }

  return (
    <div className="min-h-screen flex items-center justify-center bg-background p-4">
      <Card className="w-full max-w-md">
        <CardHeader className="text-center">
          <div className="mx-auto mb-2 flex h-12 w-12 items-center justify-center rounded-full bg-primary/10">
            {needsSetup ? (
              <Shield className="h-6 w-6 text-primary" />
            ) : (
              <KeyRound className="h-6 w-6 text-primary" />
            )}
          </div>
          <CardTitle className="text-2xl">{t('login.title')}</CardTitle>
          <CardDescription>
            {needsSetup ? t('login.setupDescription') : t('login.loginDescription')}
          </CardDescription>
        </CardHeader>
        <CardContent>
          {needsSetup ? (
            <form onSubmit={handleSetup} className="space-y-4">
              <div className="space-y-2">
                <Label htmlFor="password">{t('login.setPasswordLabel')}</Label>
                <Input
                  id="password"
                  type="password"
                  placeholder={t('login.setPasswordPlaceholder')}
                  value={password}
                  onChange={(e) => setPassword(e.target.value)}
                  disabled={loading}
                />
              </div>
              <div className="space-y-2">
                <Label htmlFor="confirm">{t('login.confirmPasswordLabel')}</Label>
                <Input
                  id="confirm"
                  type="password"
                  placeholder={t('login.confirmPasswordPlaceholder')}
                  value={confirmPassword}
                  onChange={(e) => setConfirmPassword(e.target.value)}
                  disabled={loading}
                />
              </div>
              {error && <p className="text-sm text-destructive">{error}</p>}
              <Button type="submit" className="w-full" disabled={loading || !password}>
                {loading ? t('login.settingUp') : t('login.setupButton')}
              </Button>
            </form>
          ) : (
            <form onSubmit={handleLogin} className="space-y-4">
              <div className="space-y-2">
                <Label htmlFor="password">{t('login.passwordLabel')}</Label>
                <Input
                  id="password"
                  type="password"
                  placeholder={t('login.passwordPlaceholder')}
                  value={password}
                  onChange={(e) => setPassword(e.target.value)}
                  disabled={loading}
                />
              </div>
              {error && <p className="text-sm text-destructive">{error}</p>}
              <Button type="submit" className="w-full" disabled={loading || !password}>
                {loading ? t('login.verifying') : t('login.loginButton')}
              </Button>
            </form>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
