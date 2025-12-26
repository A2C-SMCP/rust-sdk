/**
* 文件名: resource_cache
* 作者: Claude Code
* 创建日期: 2025-12-26
* 最后修改日期: 2025-12-26
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, smcp-computer
* 描述: 资源缓存管理器 / Resource cache manager
*
* ================================================================================
* 功能说明 / Functionality
* ================================================================================
*
* 本模块实现了资源缓存的管理，包括：
* - 缓存资源数据
* - TTL（Time To Live）管理
* - 缓存失效和刷新
* - 线程安全的缓存访问
*
* ================================================================================
*/

use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// 缓存的资源条目
#[derive(Debug, Clone)]
pub struct CachedResource {
    /// 资源数据
    pub data: Value,
    /// 缓存时间戳
    pub cached_at: Instant,
    /// TTL（生存时间）
    pub ttl: Duration,
    /// 版本号（每次更新递增）
    pub version: u64,
}

impl CachedResource {
    /// 创建新的缓存条目
    pub fn new(data: Value, ttl: Duration) -> Self {
        Self {
            data,
            cached_at: Instant::now(),
            ttl,
            version: 1,
        }
    }

    /// 检查缓存是否已过期
    pub fn is_expired(&self) -> bool {
        self.cached_at.elapsed() > self.ttl
    }

    /// 获取剩余 TTL
    pub fn remaining_ttl(&self) -> Duration {
        if self.is_expired() {
            Duration::ZERO
        } else {
            self.ttl.saturating_sub(self.cached_at.elapsed())
        }
    }

    /// 更新缓存数据
    pub fn refresh(&mut self, new_data: Value) {
        self.data = new_data;
        self.cached_at = Instant::now();
        self.version += 1;
    }
}

/// 资源缓存管理器
///
/// 负责管理资源数据的本地缓存，提供线程安全的缓存操作接口。
#[derive(Debug, Clone)]
pub struct ResourceCache {
    /// 缓存数据（URI -> CachedResource）
    cache: Arc<RwLock<HashMap<String, CachedResource>>>,
    /// 默认 TTL
    default_ttl: Duration,
}

impl ResourceCache {
    /// 创建新的资源缓存管理器
    ///
    /// # 参数
    /// - `default_ttl`: 默认的缓存生存时间
    pub fn new(default_ttl: Duration) -> Self {
        Self {
            cache: Arc::new(RwLock::new(HashMap::new())),
            default_ttl,
        }
    }

    /// 设置缓存
    ///
    /// # 参数
    /// - `uri`: 资源 URI
    /// - `data`: 资源数据
    /// - `ttl`: 可选的 TTL，如果为 None 则使用默认 TTL
    pub async fn set(&self, uri: String, data: Value, ttl: Option<Duration>) {
        let ttl = ttl.unwrap_or(self.default_ttl);
        let mut cache = self.cache.write().await;
        cache.insert(uri, CachedResource::new(data, ttl));
    }

    /// 获取缓存
    ///
    /// # 参数
    /// - `uri`: 资源 URI
    ///
    /// # 返回
    /// - `Some(Value)`: 缓存存在且未过期
    /// - `None`: 缓存不存在或已过期
    pub async fn get(&self, uri: &str) -> Option<Value> {
        let mut cache = self.cache.write().await;

        if let Some(cached) = cache.get(uri) {
            if !cached.is_expired() {
                return Some(cached.data.clone());
            } else {
                // 缓存已过期，移除
                cache.remove(uri);
            }
        }
        None
    }

    /// 刷新缓存（保留版本号）
    ///
    /// # 参数
    /// - `uri`: 资源 URI
    /// - `new_data`: 新的资源数据
    ///
    /// # 返回
    /// - `Ok(version)`: 刷新成功，返回新版本号
    /// - `Err(_)`: 缓存不存在
    pub async fn refresh(&self, uri: &str, new_data: Value) -> Result<u64, String> {
        let mut cache = self.cache.write().await;

        if let Some(cached) = cache.get_mut(uri) {
            cached.refresh(new_data);
            Ok(cached.version)
        } else {
            Err(format!("Resource not cached: {}", uri))
        }
    }

    /// 移除缓存
    ///
    /// # 参数
    /// - `uri`: 资源 URI
    ///
    /// # 返回
    /// - `true`: 找到并移除
    /// - `false`: 未找到
    pub async fn remove(&self, uri: &str) -> bool {
        let mut cache = self.cache.write().await;
        cache.remove(uri).is_some()
    }

    /// 清空所有缓存
    pub async fn clear(&self) {
        let mut cache = self.cache.write().await;
        cache.clear();
    }

    /// 检查资源是否已缓存且未过期
    ///
    /// # 参数
    /// - `uri`: 资源 URI
    ///
    /// # 返回
    /// - `true`: 已缓存且未过期
    /// - `false`: 未缓存或已过期
    pub async fn contains(&self, uri: &str) -> bool {
        let cache = self.cache.read().await;
        if let Some(cached) = cache.get(uri) {
            !cached.is_expired()
        } else {
            false
        }
    }

    /// 获取缓存条目的详细信息
    ///
    /// # 参数
    /// - `uri`: 资源 URI
    ///
    /// # 返回
    /// - `Some(CachedResource)`: 缓存条目
    /// - `None`: 不存在
    pub async fn get_entry(&self, uri: &str) -> Option<CachedResource> {
        let cache = self.cache.read().await;
        cache.get(uri).cloned()
    }

    /// 获取缓存大小
    pub async fn size(&self) -> usize {
        let cache = self.cache.read().await;
        cache.len()
    }

    /// 获取所有缓存的 URI 列表
    pub async fn keys(&self) -> Vec<String> {
        let cache = self.cache.read().await;
        cache.keys().cloned().collect()
    }

    /// 清理过期的缓存条目
    pub async fn cleanup_expired(&self) -> usize {
        let mut cache = self.cache.write().await;
        let initial_size = cache.len();

        cache.retain(|_, cached| !cached.is_expired());

        initial_size - cache.len()
    }
}

impl Default for ResourceCache {
    fn default() -> Self {
        Self::new(Duration::from_secs(60)) // 默认 60 秒 TTL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_set_and_get_cache() {
        let cache = ResourceCache::new(Duration::from_secs(60));

        let data = Value::String("test data".to_string());
        cache
            .set("window://test".to_string(), data.clone(), None)
            .await;

        let retrieved = cache.get("window://test").await;
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap(), data);
    }

    #[tokio::test]
    async fn test_cache_expiration() {
        let cache = ResourceCache::new(Duration::from_millis(100)); // 100ms TTL

        let data = Value::String("test data".to_string());
        cache
            .set("window://test".to_string(), data, None)
            .await;

        // 立即获取应该成功
        assert!(cache.get("window://test").await.is_some());

        // 等待过期
        tokio::time::sleep(Duration::from_millis(150)).await;

        // 过期后应该返回 None
        assert!(cache.get("window://test").await.is_none());
    }

    #[tokio::test]
    async fn test_refresh_cache() {
        let cache = ResourceCache::new(Duration::from_secs(60));

        let data1 = Value::String("version 1".to_string());
        cache
            .set("window://test".to_string(), data1.clone(), None)
            .await;

        let entry = cache.get_entry("window://test").await.unwrap();
        assert_eq!(entry.version, 1);

        let data2 = Value::String("version 2".to_string());
        cache
            .refresh("window://test", data2.clone())
            .await
            .unwrap();

        let entry = cache.get_entry("window://test").await.unwrap();
        assert_eq!(entry.version, 2);
        assert_eq!(entry.data, data2);
    }

    #[tokio::test]
    async fn test_remove_cache() {
        let cache = ResourceCache::new(Duration::from_secs(60));

        cache
            .set(
                "window://test".to_string(),
                Value::String("test".to_string()),
                None,
            )
            .await;

        assert!(cache.contains("window://test").await);

        cache.remove("window://test").await;
        assert!(!cache.contains("window://test").await);
    }

    #[tokio::test]
    async fn test_cleanup_expired() {
        let cache = ResourceCache::new(Duration::from_millis(100));

        cache
            .set(
                "window://test1".to_string(),
                Value::String("test1".to_string()),
                None,
            )
            .await;
        cache
            .set(
                "window://test2".to_string(),
                Value::String("test2".to_string()),
                Some(Duration::from_secs(60)), // 较长的 TTL
            )
            .await;

        tokio::time::sleep(Duration::from_millis(150)).await;

        let cleaned = cache.cleanup_expired().await;
        assert_eq!(cleaned, 1);

        assert!(!cache.contains("window://test1").await);
        assert!(cache.contains("window://test2").await);
    }
}
