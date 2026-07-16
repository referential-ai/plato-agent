<script lang="ts">
	import './layout.css';

	let { children } = $props();
</script>

<svelte:head>
	<title>Platonic</title>
	<meta name="theme-color" content="#f7f8fa" />
</svelte:head>
{@render children()}
