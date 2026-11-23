<script setup lang="ts">
import { count } from './types'
const formatted = count.toFixed(2)
</script>
<template>
  <div>{{ formatted.nonexistent() }}</div>
</template>
