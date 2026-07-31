import type { Metadata, Viewport } from "next";
import "./globals.css";

export const metadata: Metadata = {
  metadataBase: new URL("https://terminal.nivalis.is"),
  title: "Nivalis Terminal",
  description:
    "A Unix-style web terminal running as guest@nivalis.",
  applicationName: "Nivalis Terminal",
  authors: [{ name: "Nivalis" }],
  alternates: {
    canonical: "/",
  },
  openGraph: {
    title: "Nivalis Terminal",
    description:
      "A Unix-style web terminal running as guest@nivalis.",
    url: "/",
    siteName: "Nivalis Terminal",
    type: "website",
  },
  robots: {
    index: true,
    follow: true,
  },
};

export const viewport: Viewport = {
  width: "device-width",
  initialScale: 1,
  themeColor: "#b8babf",
};

export default function RootLayout({
  children,
}: Readonly<{
  children: React.ReactNode;
}>) {
  return (
    <html lang="en">
      <body>{children}</body>
    </html>
  );
}
