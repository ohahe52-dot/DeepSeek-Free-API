import i18n from 'i18next';
import { initReactI18next } from 'react-i18next';
import LanguageDetector from 'i18next-browser-languagedetector';

import zhTranslation from '../locales/zh/common.json';
import enTranslation from '../locales/en/common.json';
import viTranslation from '../locales/vi/common.json';

export const resources = {
  vi: {
    common: viTranslation,
  },
  zh: {
    common: zhTranslation,
  },
  en: {
    common: enTranslation,
  },
} as const;

i18n
  .use(LanguageDetector)
  .use(initReactI18next)
  .init({
    resources,
    fallbackLng: 'vi',
    debug: false,
    interpolation: {
      escapeValue: false,
    },
    defaultNS: 'common',
    detection: {
      order: ['localStorage', 'navigator'],
      caches: ['localStorage'],
      convertDetectedLanguage: (lng) => {
        const base = lng.split('-')[0];
        return base === 'vi' || base === 'zh' || base === 'en' ? base : 'vi';
      },
    },
  });

export default i18n;
