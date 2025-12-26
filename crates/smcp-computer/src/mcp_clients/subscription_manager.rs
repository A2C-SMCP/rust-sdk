/**
* 文件名: subscription_manager
* 作者: Claude Code
* 创建日期: 2025-12-26
* 最后修改日期: 2025-12-26
* 版权: 2023 JQQ. All rights reserved.
* 依赖: tokio, smcp-computer
* 描述: 资源订阅状态管理器 / Resource subscription state manager
*
* ================================================================================
* 功能说明 / Functionality
* ================================================================================
*
* 本模块实现了资源订阅的本地状态管理，包括：
* - 记录当前订阅的资源列表
* - 提供订阅状态查询接口
* - 支持订阅的添加、删除和查询
* - 线程安全的状态管理
*
* ================================================================================
*/
use crate::mcp_clients::model::Resource;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;

/// 订阅记录
#[derive(Debug, Clone)]
pub struct Subscription {
    /// 资源 URI
    pub uri: String,
    /// 订阅时间戳
    pub subscribed_at: std::time::Instant,
    /// 资源元数据
    pub resource: Resource,
}

impl Subscription {
    /// 创建新的订阅记录
    pub fn new(resource: Resource) -> Self {
        Self {
            uri: resource.uri.clone(),
            subscribed_at: std::time::Instant::now(),
            resource,
        }
    }

    /// 检查订阅是否已过期（基于 TTL）
    pub fn is_expired(&self, ttl: std::time::Duration) -> bool {
        self.subscribed_at.elapsed() > ttl
    }
}

/// 订阅状态管理器
///
/// 负责管理资源订阅的本地状态，提供线程安全的订阅管理接口。
#[derive(Debug, Clone)]
pub struct SubscriptionManager {
    /// 订阅列表（使用 Arc<RwLock> 保证线程安全）
    subscriptions: Arc<RwLock<HashSet<String>>>,
}

impl SubscriptionManager {
    /// 创建新的订阅管理器
    pub fn new() -> Self {
        Self {
            subscriptions: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    /// 添加订阅
    ///
    /// # 参数
    /// - `uri`: 资源 URI
    ///
    /// # 返回
    /// - `Ok(true)`: 新增订阅
    /// - `Ok(false)`: 已存在，无需重复订阅
    pub async fn add_subscription(&self, uri: String) -> Result<bool, String> {
        let mut subs = self.subscriptions.write().await;
        let is_new = subs.insert(uri.clone());
        Ok(is_new)
    }

    /// 移除订阅
    ///
    /// # 参数
    /// - `uri`: 资源 URI
    ///
    /// # 返回
    /// - `Ok(true)`: 找到并移除
    /// - `Ok(false)`: 未找到
    pub async fn remove_subscription(&self, uri: &str) -> Result<bool, String> {
        let mut subs = self.subscriptions.write().await;
        let removed = subs.remove(uri);
        Ok(removed)
    }

    /// 检查是否已订阅
    ///
    /// # 参数
    /// - `uri`: 资源 URI
    ///
    /// # 返回
    /// - `true`: 已订阅
    /// - `false`: 未订阅
    pub async fn is_subscribed(&self, uri: &str) -> bool {
        let subs = self.subscriptions.read().await;
        subs.contains(uri)
    }

    /// 获取所有订阅的 URI 列表
    ///
    /// # 返回
    /// - 所有订阅 URI 的向量
    pub async fn get_subscriptions(&self) -> Vec<String> {
        let subs = self.subscriptions.read().await;
        subs.iter().cloned().collect()
    }

    /// 获取订阅数量
    ///
    /// # 返回
    /// - 当前订阅总数
    pub async fn subscription_count(&self) -> usize {
        let subs = self.subscriptions.read().await;
        subs.len()
    }

    /// 清空所有订阅
    pub async fn clear(&self) {
        let mut subs = self.subscriptions.write().await;
        subs.clear();
    }

    /// 批量添加订阅
    ///
    /// # 参数
    /// - `uris`: 资源 URI 列表
    ///
    /// # 返回
    /// - 成功添加的数量
    pub async fn add_subscriptions_batch(&self, uris: Vec<String>) -> usize {
        let mut subs = self.subscriptions.write().await;
        let mut added = 0;
        for uri in uris {
            if subs.insert(uri) {
                added += 1;
            }
        }
        added
    }
}

impl Default for SubscriptionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_add_and_check_subscription() {
        let manager = SubscriptionManager::new();

        // 添加订阅
        let result = manager.add_subscription("window://test".to_string()).await;
        assert!(result.is_ok());
        assert!(result.unwrap());

        // 检查订阅
        assert!(manager.is_subscribed("window://test").await);

        // 重复添加应该返回 false
        let result = manager.add_subscription("window://test".to_string()).await;
        assert!(result.is_ok());
        assert!(!result.unwrap());
    }

    #[tokio::test]
    async fn test_remove_subscription() {
        let manager = SubscriptionManager::new();

        manager
            .add_subscription("window://test".to_string())
            .await
            .unwrap();
        assert!(manager.is_subscribed("window://test").await);

        // 移除订阅
        let removed = manager.remove_subscription("window://test").await.unwrap();
        assert!(removed);
        assert!(!manager.is_subscribed("window://test").await);

        // 再次移除应该返回 false
        let removed = manager.remove_subscription("window://test").await.unwrap();
        assert!(!removed);
    }

    #[tokio::test]
    async fn test_get_subscriptions() {
        let manager = SubscriptionManager::new();

        manager
            .add_subscription("window://test1".to_string())
            .await
            .unwrap();
        manager
            .add_subscription("window://test2".to_string())
            .await
            .unwrap();

        let subs = manager.get_subscriptions().await;
        assert_eq!(subs.len(), 2);
        assert!(subs.contains(&"window://test1".to_string()));
        assert!(subs.contains(&"window://test2".to_string()));
    }

    #[tokio::test]
    async fn test_clear_subscriptions() {
        let manager = SubscriptionManager::new();

        manager
            .add_subscription("window://test1".to_string())
            .await
            .unwrap();
        manager
            .add_subscription("window://test2".to_string())
            .await
            .unwrap();

        assert_eq!(manager.subscription_count().await, 2);

        manager.clear().await;
        assert_eq!(manager.subscription_count().await, 0);
    }
}
