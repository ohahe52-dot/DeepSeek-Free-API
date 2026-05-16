import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Languages } from 'lucide-react';

export function LanguageSwitcher() {
  const { i18n, t } = useTranslation();

  const toggleLanguage = () => {
    const currentLang = i18n.resolvedLanguage ?? i18n.language;
    const order = ['vi', 'zh', 'en'];
    const currentIndex = order.indexOf(currentLang);
    const nextLang = order[(currentIndex + 1) % order.length] ?? 'vi';
    i18n.changeLanguage(nextLang);
  };

  const currentLang = i18n.resolvedLanguage ?? i18n.language;
  const nextLabelKey =
    currentLang === 'vi' ? 'language.zh' : currentLang === 'zh' ? 'language.en' : 'language.vi';

  return (
    <Button
      variant="ghost"
      size="sm"
      onClick={toggleLanguage}
      className="w-full justify-start gap-3 text-muted-foreground"
    >
      <Languages className="h-4 w-4" />
      {t(nextLabelKey)}
    </Button>
  );
}
