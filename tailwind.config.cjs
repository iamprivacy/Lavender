/** @type {import('tailwindcss').Config} */
module.exports = {
  content: ['./src/**/*.{html,js,vue}'],
  theme: {
    extend: {
      colors: {
        // Lavender: the upstream name hoa is kept so no class names change;
        // the values are upstream's pink scale moved to a lavender hue at the
        // same lightness, so contrast matches upstream.
        hoa: {
          0: '#e9e1f9',
          100: '#e8e0f9',
          200: '#e5dcf9',
          300: '#e1d6f8',
          400: '#dbcef7',
          500: '#d5c5f6',
          600: '#cdb8f4',
          700: '#c5acf4',
          800: '#bc9ef2',
          900: '#b592f1',
          1000: '#af87ef',
          1100: '#a87eeb',
          1200: '#a175e4',
          1300: '#966bd7',
          1400: '#855ec1',
          1500: '#714fa6',
        },
      },
    },
  },
  darkMode: 'class',
}
