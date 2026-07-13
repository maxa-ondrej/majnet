// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

// https://astro.build/config
export default defineConfig({
	site: 'https://majksa.net',
	integrations: [
		starlight({
			title: 'MajNet',
			description: 'A self-hosted, GitOps-driven deployment platform built on plain Docker.',
			social: [
				{ icon: 'github', label: 'GitHub', href: 'https://github.com/maxa-ondrej/majnet' },
			],
			sidebar: [
				{
					label: 'Start here',
					items: [{ label: 'Overview', slug: 'overview' }],
				},
				{
					label: 'How it works',
					items: [
						{ label: 'Architecture', slug: 'architecture' },
						{ label: 'GitHub model', slug: 'github-model' },
						{ label: 'Environment classes', slug: 'environment-classes' },
					],
				},
			],
		}),
	],
});
